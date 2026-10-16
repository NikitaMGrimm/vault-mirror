use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::OnceLock,
    time::{Duration, Instant},
};

mod console;
mod wizard;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use console::{Console, ERROR};
use time::OffsetDateTime;
use vault_mirror::{
    api::build_http_client,
    app::{self, RunFlags},
    config::{self, Config},
    mirror::{Endpoint, MirrorOptions, mirror},
    restore::{RestoreOptions, restore},
    schedule::Timetable,
    snapshot,
    store::{self, Keys, Snapshot, Store},
    text::count,
};

/// Mirror and back up a Bitwarden or Vaultwarden vault.
///
/// Configuration comes from environment variables or --env-file; `init`
/// writes one.
#[derive(Parser)]
#[command(name = "vault-mirror", version, max_term_width = 100)]
struct Cli {
    /// Read settings from this file of NAME=value lines, as `init` writes
    /// it. The environment takes precedence.
    #[arg(long, global = true, value_name = "PATH")]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone, Copy)]
struct RunArgs {
    /// Let this run delete every earlier copy, or more than MAX_DELETE_PERCENT.
    #[arg(long)]
    allow_mass_delete: bool,
    /// Let this run take over copies made the other way round, after you switched the source and destination.
    #[arg(long)]
    reverse: bool,
    /// First copy what changed in the destination back into the source, such as after using the destination while the source was down.
    #[arg(long)]
    pull: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Ask for the accounts, check the logins, and write vault-mirror.env and compose.yaml.
    Init {
        /// Directory to write the files to.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Back up, then mirror the source into the destination, once.
    Run(RunArgs),
    /// Keep running on SCHEDULE.
    Daemon,
    /// Log in to the configured vaults and report what a run would do, without changing them.
    Check,
    /// List the snapshots in the backup directory.
    Snapshots {
        /// Whose backups to list.
        #[arg(long, value_enum, default_value_t = Role::Source)]
        from: Role,
    },
    /// Write a snapshot into a vault.
    Restore(RestoreArgs),
    /// Decrypt a snapshot and its attachments into a directory, in the clear.
    Export {
        #[command(flatten)]
        snapshot: SnapshotArgs,
        /// Directory to write to. It must not exist yet.
        #[arg(long)]
        output: PathBuf,
    },
    /// Exit successfully only if the last run succeeded recently. For container health checks.
    Healthcheck {
        /// The oldest a successful run may be, in hours.
        #[arg(long, default_value_t = 26)]
        max_age_hours: u64,
    },
}

#[derive(Args)]
struct SnapshotArgs {
    /// Whose backups to read from.
    #[arg(long, value_enum, default_value_t = Role::Source)]
    from: Role,
    /// A snapshot id, as `snapshots` lists it, or `latest`.
    #[arg(long, default_value = "latest")]
    snapshot: String,
}

#[derive(Args)]
struct RestoreArgs {
    #[command(flatten)]
    snapshot: SnapshotArgs,
    /// The configured vault to restore into.
    #[arg(long, value_enum)]
    to: Role,
    /// Restore into a vault that already has items, keeping them.
    #[arg(long)]
    allow_non_empty: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Role {
    Source,
    Dest,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Dest => "dest",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            anstream::eprintln!("{ERROR}error:{ERROR:#} could not start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(error) => {
            Console::clear_progress();
            anstream::eprintln!("{ERROR}error:{ERROR:#} {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Settings read with `--env-file`.
static ENV_FILE: OnceLock<HashMap<String, String>> = OnceLock::new();

/// The environment, over the settings read with `--env-file`.
fn settings() -> HashMap<String, String> {
    let mut env = ENV_FILE.get().cloned().unwrap_or_default();
    // `std::env::vars` panics on a variable that is not UTF-8, even one that
    // vault-mirror never reads. Such a variable cannot be a valid setting.
    env.extend(
        std::env::vars_os().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }),
    );
    env
}

fn config() -> Result<Config> {
    Config::from_env(&settings()).map_err(|error| anyhow!("configuration: {error}"))
}

/// Reads `NAME=value` lines literally, as Docker Compose's `format: raw`
/// does: no quotes, no variables, no comments after a value. Blank lines
/// and lines starting with `#` are skipped.
fn parse_env_file(text: &str) -> Result<HashMap<String, String>> {
    let mut settings = HashMap::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("line {} is not NAME=value", number + 1))?;
        settings.insert(name.trim().to_string(), value.to_string());
    }
    Ok(settings)
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    if let Some(path) = &cli.env_file {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let settings =
            parse_env_file(&text).with_context(|| format!("reading {}", path.display()))?;
        ENV_FILE.get_or_init(|| settings);
    }
    match cli.command {
        Command::Init { dir } => wizard::init(&dir).await,
        Command::Run(args) => run(args).await,
        Command::Daemon => daemon().await,
        Command::Check => check().await,
        Command::Snapshots { from } => snapshots(from),
        Command::Restore(args) => restore_command(args).await,
        Command::Export { snapshot, output } => export(snapshot, &output),
        Command::Healthcheck { max_age_hours } => healthcheck(max_age_hours),
    }
}

fn flags(args: RunArgs) -> RunFlags {
    RunFlags {
        allow_mass_delete: args.allow_mass_delete,
        reverse: args.reverse,
        pull: args.pull,
    }
}

async fn run_and_report(config: &Config, flags: RunFlags) -> bool {
    let status = app::run_once(config, flags, &Console).await;
    if status.succeeded() {
        Console::finished(&finished(&status), !status.warnings.is_empty());
    } else {
        Console::error(&match (&status.failure_stage, &status.error) {
            (Some(stage), Some(error)) => format!("the run failed during {stage}: {error}"),
            _ => app::describe(&status),
        });
    }
    status.succeeded()
}

/// What a successful run did. Each kind of run has one fixed shape, so the
/// last line reads the same every time; details such as the snapshot name
/// are on the lines above.
fn finished(status: &app::Status) -> String {
    let seconds = status.duration_seconds;
    match &status.mirror {
        Some(app::SummaryRecord {
            summary: mirror, ..
        }) => {
            let pulled = match &status.pulled {
                Some(app::SummaryRecord { summary, .. }) => format!(
                    "{} created and {} updated in the source, then ",
                    summary.items_created, summary.items_updated
                ),
                None => String::new(),
            };
            format!(
                "{pulled}{} created, {} updated, {} deleted, {} unchanged in {seconds:.1}s",
                mirror.items_created,
                mirror.items_updated,
                mirror.items_deleted,
                mirror.items_unchanged
            )
        }
        None => format!("backup in {seconds:.1}s"),
    }
}

async fn run(args: RunArgs) -> Result<ExitCode> {
    let config = config()?;
    Ok(if run_and_report(&config, flags(args)).await {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn daemon() -> Result<ExitCode> {
    let config = config()?;
    let schedule = (config.schedule.as_deref())
        .ok_or_else(|| anyhow!("configuration: the daemon needs SCHEDULE, a cron expression"))?;
    let timetable = Timetable::new(schedule, config.timezone.as_deref())?;
    let mut terminate = Terminate::new()?;
    loop {
        let next = timetable.next(chrono::Utc::now())?;
        Console::status(
            "Waiting",
            &format!(
                "for the next run at {}",
                next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
        );
        let wait = (next - chrono::Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
        // A run in progress is finished, not interrupted: the signal is
        // only looked at between runs.
        run_and_report(&config, RunFlags::default()).await;
    }
    Console::status("Stopping", "the daemon");
    Ok(ExitCode::SUCCESS)
}

/// The signal Docker and systemd stop the daemon with. Windows has none.
#[cfg(unix)]
struct Terminate(tokio::signal::unix::Signal);

#[cfg(unix)]
impl Terminate {
    fn new() -> std::io::Result<Self> {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map(Self)
    }

    async fn recv(&mut self) {
        self.0.recv().await;
    }
}

#[cfg(not(unix))]
struct Terminate;

#[cfg(not(unix))]
impl Terminate {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) {
        std::future::pending().await
    }
}

async fn check() -> Result<ExitCode> {
    check_config(&config()?, true).await?;
    Ok(ExitCode::SUCCESS)
}

/// Item, folder, collection and attachment counts, leaving out zeros
/// except for items.
fn describe_vault(vault: &vault_mirror::vault::Vault) -> String {
    let live: Vec<_> = vault.items.iter().filter(|item| !item.in_trash).collect();
    let attachments = live.iter().map(|item| item.attachments.len()).sum();
    let mut parts = vec![count(live.len(), "item")];
    for (n, noun) in [
        (vault.folders.len(), "folder"),
        (vault.collections.len(), "collection"),
        (attachments, "attachment"),
    ] {
        if n > 0 {
            parts.push(count(n, noun));
        }
    }
    parts.join(", ")
}

/// Logs in to the configured vaults and reports what a run would do.
/// `init` has reported the logins already.
async fn check_config(config: &Config, report_logins: bool) -> Result<()> {
    let started = Instant::now();
    let http = app::http_client(config)?;
    let source = app::open_account(&config.backups_dir, &http, &config.source, "source").await?;
    let source_scope = app::scope_of(&config.source);
    if report_logins {
        Console::status(
            "Logged in",
            &format!(
                "to the source: {}",
                describe_vault(&source.load(&source_scope).await?)
            ),
        );
    }
    let Some(dest_config) = &config.dest else {
        Console::note("no destination is configured, so runs only back up the source");
        Console::finished(&checked(started), false);
        return Ok(());
    };
    let dest = app::open_account(&config.backups_dir, &http, dest_config, "dest").await?;
    let dest_scope = app::scope_of(dest_config);
    if report_logins {
        Console::status(
            "Logged in",
            &format!(
                "to the destination: {}",
                describe_vault(&dest.load(&dest_scope).await?)
            ),
        );
    }

    let options = MirrorOptions {
        dry_run: true,
        safety: config.safety.clone(),
        filter: config.filter.clone(),
    };
    let source = Endpoint {
        account: &source,
        scope: source_scope,
    };
    let dest = Endpoint {
        account: &dest,
        scope: dest_scope,
    };
    let outcome = mirror(&source, &dest, &options, &Console).await?;
    Console::finished(&checked(started), !outcome.warnings.is_empty());
    Ok(())
}

fn checked(started: Instant) -> String {
    format!(
        "check in {:.1}s; neither vault was changed",
        started.elapsed().as_secs_f64()
    )
}

fn read_keys(backups: &Path, env: &HashMap<String, String>) -> Result<Keys> {
    let passphrase = config::backup_passphrase(env)?
        .ok_or_else(|| anyhow!("reading a backup needs BACKUP_PASSPHRASE"))?;
    Keys::from_key_file(backups, &passphrase)
}

fn snapshots(vault: Role) -> Result<ExitCode> {
    let backups = config::backups_dir(&settings());
    let _lock = app::lock_backups_dir(&backups)?;
    let ids = store::list(&backups, vault.name())?;
    if ids.is_empty() {
        println!(
            "No snapshots of the {} vault in {}.",
            vault.name(),
            backups.display()
        );
    }
    for id in ids {
        println!("{id}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The snapshot `name`, as `snapshots` lists it, or the newest for `latest`.
fn locate_snapshot<'a>(store: &Store<'a>, role: Role, name: &str) -> Result<Snapshot<'a>> {
    let ids = store.list()?;
    let id = match name {
        "latest" => ids.last(),
        name => ids.iter().find(|id| *id == name),
    };
    let id = id.ok_or_else(|| anyhow!("no snapshot {name} of the {} vault", role.name()))?;
    Ok(store.snapshot(id))
}

async fn restore_command(args: RestoreArgs) -> Result<ExitCode> {
    let started = Instant::now();
    let env = settings();
    let backups = config::backups_dir(&env);
    // A run at the same time could prune the snapshot being restored, or
    // mirror into the vault being restored to.
    let _lock = app::lock_backups_dir(&backups)?;
    let keys = read_keys(&backups, &env)?;
    let store = Store::new(&backups, args.snapshot.from.name(), &keys);
    let backup = locate_snapshot(&store, args.snapshot.from, &args.snapshot.snapshot)?;

    let http = build_http_client(&config::client_options(&env)?)?;
    let role = args.to.name();
    let target_config = config::vault_config(&env, &role.to_uppercase())?;
    let account = app::open_account(&backups, &http, &target_config, role).await?;
    let target = Endpoint {
        account: &account,
        scope: app::scope_of(&target_config),
    };
    let options = RestoreOptions {
        allow_non_empty: args.allow_non_empty,
    };
    let report = restore(&target, &backup, &options, &Console).await?;
    Console::finished(
        &format!(
            "restore of {}, {}, {} and {} in {:.1}s",
            count(report.items, "item"),
            count(report.attachments_restored, "attachment"),
            count(report.folders_created, "new folder"),
            count(report.collections_created, "new collection"),
            started.elapsed().as_secs_f64()
        ),
        report.items_left_out + report.items_left_unarchived + report.attachments_skipped > 0,
    );
    Ok(ExitCode::SUCCESS)
}

fn export(args: SnapshotArgs, output: &Path) -> Result<ExitCode> {
    let env = settings();
    let backups = config::backups_dir(&env);
    let _lock = app::lock_backups_dir(&backups)?;
    let keys = read_keys(&backups, &env)?;
    let store = Store::new(&backups, args.from.name(), &keys);
    let backup = locate_snapshot(&store, args.from, &args.snapshot)?;
    let document = backup.document()?;
    let restorable = snapshot::parse(&document)?;
    if output.exists() {
        bail!("{} already exists", output.display());
    }
    // A damaged backup exports nothing.
    for attachment in restorable.attachments() {
        backup.attachment(&attachment.attachment_id)?;
    }

    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut directory, 0o700);
    directory
        .create(output)
        .with_context(|| format!("creating {}", output.display()))?;
    fs::write(
        output.join("export.json"),
        serde_json::to_vec_pretty(&document)?,
    )?;
    let mut written = 0;
    for attachment in restorable.attachments() {
        // The attachment id keeps files with the same name apart; the name
        // itself is reduced to its final path component.
        let name = Path::new(&attachment.file_name)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());
        let directory = output.join("attachments").join(&attachment.attachment_id);
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join(name),
            backup.attachment(&attachment.attachment_id)?,
        )?;
        written += 1;
    }
    Console::warning(&format!(
        "{} is not encrypted. Any Bitwarden client can import its export.json. Delete it when you are done.",
        output.display()
    ));
    Console::finished(
        &format!(
            "export of {} and {} to {}",
            count(restorable.items().len(), "item"),
            count(written, "attachment"),
            output.display()
        ),
        true,
    );
    Ok(ExitCode::SUCCESS)
}

fn healthcheck(max_age_hours: u64) -> Result<ExitCode> {
    let max_age = Duration::from_secs(
        max_age_hours
            .checked_mul(3600)
            .ok_or_else(|| anyhow!("--max-age-hours is too large"))?,
    );
    let status = app::read_status(&config::backups_dir(&settings()))?;
    match app::health(&status, max_age, OffsetDateTime::now_utc()) {
        Ok(()) => {
            println!("healthy: {}", app::describe(&status));
            Ok(ExitCode::SUCCESS)
        }
        Err(reason) => {
            println!("unhealthy: {reason}");
            Ok(ExitCode::FAILURE)
        }
    }
}
