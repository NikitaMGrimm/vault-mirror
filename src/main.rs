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
    app::{self, RunFlags},
    config::{self, Config, VaultConfig},
    mirror::{Endpoint, MirrorOptions, mirror},
    restore::{RestoreOptions, restore},
    schedule::Timetable,
    snapshot,
    store::{Keys, Store},
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
    /// Back up and report what would change, without changing the destination.
    #[arg(long)]
    dry_run: bool,
    /// Let this run delete every earlier copy, or more than MAX_DELETE_PERCENT.
    #[arg(long)]
    allow_mass_delete: bool,
    /// Let this run take over copies made the other way round, after you switched the source and destination.
    #[arg(long)]
    reverse: bool,
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
    /// Keep running on SCHEDULE or every INTERVAL.
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
    /// A snapshot file or name, as `snapshots` lists it, or `latest`.
    #[arg(long, default_value = "latest")]
    snapshot: String,
    /// An age identity file, needed when backups are made with BACKUP_RECIPIENTS.
    #[arg(long)]
    identity: Option<PathBuf>,
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
        dry_run: args.dry_run,
        allow_mass_delete: args.allow_mass_delete,
        reverse: args.reverse,
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
        _ if status.dry_run => format!("dry run in {seconds:.1}s; the destination is unchanged"),
        Some(app::SummaryRecord {
            summary: mirror, ..
        }) => format!(
            "{} created, {} updated, {} deleted, {} unchanged in {seconds:.1}s",
            mirror.items_created,
            mirror.items_updated,
            mirror.items_deleted,
            mirror.items_unchanged
        ),
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
    let schedule = config.schedule.as_ref().ok_or_else(|| {
        anyhow!(
            "configuration: the daemon needs SCHEDULE (a cron expression) or INTERVAL (such as 6h)"
        )
    })?;
    let timetable = Timetable::new(schedule, config.timezone.as_deref())?;
    let mut terminate = Terminate::new()?;

    if timetable.runs_at_start() {
        run_and_report(&config, RunFlags::default()).await;
    }
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
    // `init` warned before each login already.
    if report_logins {
        for warning in config
            .vaults()
            .filter_map(|(vault, role)| app::plain_http_warning(vault, role))
        {
            Console::warning(&warning);
        }
    }
    let http = app::http_client(config)?;
    let source = app::open_account(config, &http, &config.source, "source").await?;
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
    let dest = app::open_account(config, &http, dest_config, "dest").await?;
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
        ..MirrorOptions::default()
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

fn backup_keys(config: &Config, identity: Option<&Path>) -> Result<Keys> {
    let backup = config
        .backup
        .as_ref()
        .ok_or_else(|| anyhow!("backups are switched off (BACKUP_ENABLED=false)"))?;
    let keys = Keys::open(&config.backups_dir, &backup.protection)?;
    match identity {
        Some(path) => keys.with_identity_file(path),
        None => Ok(keys),
    }
}

fn snapshots(vault: Role) -> Result<ExitCode> {
    let config = config()?;
    let keys = backup_keys(&config, None)?;
    let store = Store::open(&config.backups_dir, vault.name(), &keys)?;
    let list = store.list()?;
    if list.is_empty() {
        println!(
            "No snapshots of the {} vault in {}.",
            vault.name(),
            config.backups_dir.display()
        );
    }
    for snapshot in list {
        let detail = match &snapshot.meta {
            Some(meta) => format!(
                "{}, {}",
                count(meta.items, "item"),
                count(meta.attachments.len(), "attachment file")
            ),
            None => "no metadata".to_string(),
        };
        println!("{}  {detail}", snapshot.path.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn locate_snapshot(store: &Store<'_>, role: Role, snapshot: &str) -> Result<PathBuf> {
    if snapshot != "latest" {
        let path = PathBuf::from(snapshot);
        if path.exists() {
            return Ok(path);
        }
        // A file name as `snapshots` lists it.
        return store
            .list()?
            .into_iter()
            .find(|entry| entry.path.file_name() == Some(std::ffi::OsStr::new(snapshot)))
            .map(|entry| entry.path)
            .ok_or_else(|| anyhow!("no snapshot {snapshot} of the {} vault", role.name()));
    }
    store
        .list()?
        .pop()
        .map(|latest| latest.path)
        .ok_or_else(|| anyhow!("there are no snapshots of the {} vault", role.name()))
}

fn vault_config(config: &Config, role: Role) -> Result<&VaultConfig> {
    match role {
        Role::Source => Ok(&config.source),
        Role::Dest => config
            .dest
            .as_ref()
            .ok_or_else(|| anyhow!("no destination vault is configured")),
    }
}

async fn restore_command(args: RestoreArgs) -> Result<ExitCode> {
    let started = Instant::now();
    let config = config()?;
    // A run at the same time could prune the snapshot being restored, or
    // mirror into the vault being restored to.
    let _lock = app::lock_backups_dir(&config.backups_dir)?;
    let keys = backup_keys(&config, args.snapshot.identity.as_deref())?;
    let store = Store::open(&config.backups_dir, args.snapshot.from.name(), &keys)?;
    let path = locate_snapshot(&store, args.snapshot.from, &args.snapshot.snapshot)?;
    let restorable = snapshot::parse(&store.read_snapshot(&path)?)?;
    Console::status(
        "Reading",
        &format!(
            "{} from {}",
            count(restorable.items.len(), "item"),
            path.display()
        ),
    );

    let http = app::http_client(&config)?;
    let target_config = vault_config(&config, args.to)?;
    if let Some(warning) = app::plain_http_warning(target_config, args.to.name()) {
        Console::warning(&warning);
    }
    let account = app::open_account(&config, &http, target_config, args.to.name()).await?;
    let target = Endpoint {
        account: &account,
        scope: app::scope_of(target_config),
    };
    let options = RestoreOptions {
        allow_non_empty: args.allow_non_empty,
        concurrency: config.concurrency,
    };
    let report = restore(&target, &restorable, Some(&store), &options, &Console).await?;
    let warned = report.items_left_out > 0
        || report.attachments_skipped > 0
        || report.attachments_missing > 0;
    Console::finished(
        &format!(
            "restore of {}, {}, {} and {} in {:.1}s",
            count(report.items, "item"),
            count(report.attachments_restored, "attachment"),
            count(report.folders_created, "new folder"),
            count(report.collections_created, "new collection"),
            started.elapsed().as_secs_f64()
        ),
        warned,
    );
    Ok(ExitCode::SUCCESS)
}

fn export(args: SnapshotArgs, output: &Path) -> Result<ExitCode> {
    let config = config()?;
    let keys = backup_keys(&config, args.identity.as_deref())?;
    let store = Store::open(&config.backups_dir, args.from.name(), &keys)?;
    let path = locate_snapshot(&store, args.from, &args.snapshot)?;
    let document = store.read_snapshot(&path)?;
    let restorable = snapshot::parse(&document)?;

    if output.exists() {
        bail!("{} already exists", output.display());
    }
    fs::create_dir_all(output).with_context(|| format!("creating {}", output.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    }
    fs::write(
        output.join("export.json"),
        serde_json::to_vec_pretty(&document)?,
    )?;
    let mut written = 0;
    let mut missing = 0;
    for item in &restorable.items {
        for attachment in &item.attachments {
            if !(attachment.stored && store.has_attachment(&attachment.attachment_id)) {
                missing += 1;
                continue;
            }
            // Item and attachment ids keep files with the same name apart;
            // the name itself is reduced to its final path component.
            let name = Path::new(&attachment.file_name)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "attachment".to_string());
            let directory = output
                .join("attachments")
                .join(safe(&item.id))
                .join(safe(&attachment.attachment_id));
            fs::create_dir_all(&directory)?;
            fs::write(
                directory.join(name),
                store.read_attachment(&attachment.attachment_id)?,
            )?;
            written += 1;
        }
    }
    if missing > 0 {
        Console::warning(&format!(
            "The backup lacks the content of {} listed in the snapshot.",
            count(missing, "attachment")
        ));
    }
    Console::warning(&format!(
        "{} is not encrypted. Any Bitwarden client can import its export.json. Delete it when you are done.",
        output.display()
    ));
    Console::finished(
        &format!(
            "export of {} and {} to {}",
            count(restorable.items.len(), "item"),
            count(written, "attachment"),
            output.display()
        ),
        true,
    );
    Ok(ExitCode::SUCCESS)
}

fn safe(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn healthcheck(max_age_hours: u64) -> Result<ExitCode> {
    let status = app::read_status(&config::backups_dir(&settings()))?;
    match app::health(
        &status,
        Duration::from_secs(max_age_hours * 3600),
        OffsetDateTime::now_utc(),
    ) {
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
