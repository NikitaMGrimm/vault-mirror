use std::{
    collections::HashMap,
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

mod wizard;

use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use time::OffsetDateTime;
use vault_mirror::{
    app::{self, RunFlags},
    config::{Config, VaultConfig},
    mirror::{Endpoint, MirrorOptions, Reporter, mirror},
    restore::{RestoreOptions, restore},
    schedule::Timetable,
    snapshot,
    store::{Keys, Store},
    text::count,
};

/// Mirror and back up a Bitwarden or Vaultwarden vault.
///
/// Configuration comes from environment variables or --env-file; `init`
/// writes one. Without a subcommand, one run is performed.
#[derive(Parser)]
#[command(name = "vault-mirror", version, max_term_width = 100)]
struct Cli {
    /// Read settings from this file of NAME=value lines, as `init` writes
    /// it. The environment takes precedence.
    #[arg(long, global = true, value_name = "PATH")]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Args, Clone, Copy, Default)]
struct RunArgs {
    /// Back up and report what would change, without changing the destination.
    #[arg(long)]
    dry_run: bool,
    /// Let the first run delete items that are already in the destination.
    #[arg(long)]
    allow_initial_overwrite: bool,
    /// Let this run delete every earlier copy, or more than MAX_DELETE_PERCENT.
    #[arg(long)]
    allow_mass_delete: bool,
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
    /// Log in to the configured vaults and report what a run would do. Writes nothing.
    Check,
    /// List the snapshots in the backup directory.
    Snapshots {
        /// Whose backups to list.
        #[arg(long, alias = "vault", value_enum, default_value_t = Role::Source)]
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
    /// Create throwaway accounts on test servers and print their settings.
    #[command(hide = true)]
    Sandbox {
        #[arg(long, default_value = "http://127.0.0.1:18081")]
        source: String,
        #[arg(long, default_value = "http://127.0.0.1:18082")]
        dest: String,
        /// Fill the source vault with sample items.
        #[arg(long)]
        seed: bool,
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

/// Writes progress for people at a terminal, in the style of Cargo: a
/// right-aligned verb, then the rest of the sentence. Elsewhere, such as in
/// container logs, each line is a timestamped log line instead.
struct Console;

const VERB: Style = AnsiColor::Green.on_default().bold();
const PROGRESS: Style = AnsiColor::Cyan.on_default().bold();
const WARNING: Style = AnsiColor::Yellow.on_default().bold();
const ERROR: Style = AnsiColor::Red.on_default().bold();
const NOTE: Style = AnsiColor::Cyan.on_default().bold();

/// Whether a progress bar occupies the terminal's last line.
static PROGRESS_SHOWN: AtomicBool = AtomicBool::new(false);

impl Console {
    fn terminal() -> bool {
        std::io::stdout().is_terminal()
    }

    /// Removes the progress bar so that a line can take its place.
    fn clear_progress() {
        if PROGRESS_SHOWN.swap(false, Ordering::Relaxed) {
            // Written directly: anstream would strip the control sequence
            // along with colours when NO_COLOR is set.
            eprint!("\r\x1b[K");
        }
    }

    fn status(verb: &str, detail: &str) {
        if !Self::terminal() {
            return Self::log("INFO", &format!("{verb} {detail}"));
        }
        Self::clear_progress();
        anstream::println!("{VERB}{verb:>12}{VERB:#} {detail}");
    }

    fn warning(message: &str) {
        if !Self::terminal() {
            return Self::log("WARN", message);
        }
        Self::clear_progress();
        anstream::println!("{WARNING}warning:{WARNING:#} {message}");
    }

    fn note(message: &str) {
        if !Self::terminal() {
            return Self::log("INFO", message);
        }
        Self::clear_progress();
        anstream::println!("{NOTE}note:{NOTE:#} {message}");
    }

    fn error(message: &str) {
        if !Self::terminal() {
            return Self::log("ERROR", message);
        }
        Self::clear_progress();
        anstream::println!("{ERROR}error:{ERROR:#} {message}");
    }

    fn log(level: &str, message: &str) {
        let now = OffsetDateTime::now_utc();
        println!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z {level} {message}",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
    }
}

impl Reporter for Console {
    fn status(&self, verb: &str, detail: &str) {
        Self::status(verb, detail);
    }
    fn warn(&self, message: &str) {
        Self::warning(message);
    }
    fn progress(&self, verb: &str, done: usize, total: usize, unit: &str) {
        const WIDTH: usize = 25;
        if !Self::terminal() || !std::io::stderr().is_terminal() {
            return;
        }
        if done >= total {
            return Self::clear_progress();
        }
        let filled = WIDTH * done / total;
        let bar = format!("{}>{}", "=".repeat(filled), " ".repeat(WIDTH - filled - 1));
        eprint!("\r\x1b[K");
        anstream::eprint!("{PROGRESS}{verb:>12}{PROGRESS:#} [{bar}] {done}/{total} {unit}");
        PROGRESS_SHOWN.store(true, Ordering::Relaxed);
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

/// Run options belong after `run`, or stand alone without a subcommand.
/// Before a subcommand they would be ignored, so they are rejected. Clap's
/// own rule for this would also reject `--env-file` there.
fn check_placement(cli: &Cli) -> Result<()> {
    let RunArgs {
        dry_run,
        allow_initial_overwrite,
        allow_mass_delete,
    } = cli.run;
    if (dry_run || allow_initial_overwrite || allow_mass_delete) && cli.command.is_some() {
        bail!("--dry-run and the --allow options belong after `run`");
    }
    Ok(())
}

/// Settings read with `--env-file`.
static ENV_FILE: OnceLock<HashMap<String, String>> = OnceLock::new();

fn config() -> Result<Config> {
    let mut env = ENV_FILE.get().cloned().unwrap_or_default();
    env.extend(std::env::vars());
    Config::from_env(&env).map_err(|error| anyhow!("configuration: {error}"))
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
    check_placement(&cli)?;
    if let Some(path) = &cli.env_file {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let settings =
            parse_env_file(&text).with_context(|| format!("reading {}", path.display()))?;
        ENV_FILE.get_or_init(|| settings);
    }
    match cli.command {
        Some(Command::Init { dir }) => wizard::init(&dir).await,
        None => run(cli.run).await,
        Some(Command::Run(args)) => run(args).await,
        Some(Command::Daemon) => daemon().await,
        Some(Command::Check) => check().await,
        Some(Command::Snapshots { from }) => snapshots(from),
        Some(Command::Restore(args)) => restore_command(args).await,
        Some(Command::Export { snapshot, output }) => export(snapshot, &output),
        Some(Command::Healthcheck { max_age_hours }) => healthcheck(max_age_hours),
        Some(Command::Sandbox { source, dest, seed }) => sandbox(&source, &dest, seed).await,
    }
}

fn flags(args: RunArgs) -> RunFlags {
    RunFlags {
        dry_run: args.dry_run,
        allow_initial_overwrite: args.allow_initial_overwrite,
        allow_mass_delete: args.allow_mass_delete,
    }
}

async fn run_and_report(config: &Config, flags: RunFlags) -> bool {
    let status = app::run_once(config, flags, &Console).await;
    if status.succeeded() {
        Console::status("Finished", &finished(&status));
    } else {
        Console::error(&app::describe(&status));
    }
    status.succeeded()
}

/// What a successful run did, in a few words.
fn finished(status: &app::Status) -> String {
    let mut text = String::new();
    if status.dry_run {
        text.push_str("dry run: ");
    }
    match &status.mirror {
        Some(mirror) => text.push_str(&format!(
            "{} created, {} updated, {} deleted, {} unchanged",
            mirror.items_created,
            mirror.items_updated,
            mirror.items_deleted,
            mirror.items_unchanged
        )),
        None => text.push_str("backup"),
    }
    text.push_str(&format!(" in {:.1}s", status.duration_seconds));
    if !status.warnings.is_empty() {
        text.push_str(&format!(
            " with {}",
            count(status.warnings.len(), "warning")
        ));
    }
    text
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
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

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
        Console::status(
            "Finished",
            "check; no destination is configured, so runs only back up the source",
        );
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
    mirror(&source, &dest, &options, &Console).await?;
    Ok(())
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
    Console::status(
        "Restored",
        &format!(
            "{} and {}; created {} and {}",
            count(report.items, "item"),
            count(report.attachments_restored, "attachment"),
            count(report.folders_created, "folder"),
            count(report.collections_created, "collection")
        ),
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
    Console::status(
        "Exported",
        &format!(
            "{} and {} to {}, unencrypted. Any Bitwarden client can import export.json",
            count(restorable.items.len(), "item"),
            count(written, "attachment"),
            output.display()
        ),
    );
    if missing > 0 {
        Console::warning(&format!(
            "The backup lacks the content of {} listed in the snapshot.",
            count(missing, "attachment")
        ));
    }
    Ok(ExitCode::SUCCESS)
}

fn safe(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn healthcheck(max_age_hours: u64) -> Result<ExitCode> {
    let backups_dir = std::env::var("BACKUPS_DIR").unwrap_or_else(|_| "backups".into());
    let status = app::read_status(Path::new(&backups_dir))?;
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

async fn sandbox(source: &str, dest: &str, seed: bool) -> Result<ExitCode> {
    use vault_mirror::{api, crypto::KdfParams, fixtures, provision, vault};

    let http = api::build_http_client(&api::ClientOptions::default())?;
    let kdf = KdfParams::Pbkdf2 {
        iterations: 600_000,
    };
    let password = "sandbox-master-password";
    let mut lines = Vec::new();
    for (prefix, server) in [("SOURCE", source), ("DEST", dest)] {
        let email = format!("sandbox-{}@example.com", uuid::Uuid::new_v4().simple());
        let account = provision::register(&http, server, &email, password, &kdf).await?;
        if seed && prefix == "SOURCE" {
            let urls = api::ServerUrls::from_server(server)?;
            let opened =
                vault::Account::open(http.clone(), urls, account.credentials.clone(), password)
                    .await?;
            let mut folders = HashMap::new();
            for name in fixtures::FOLDERS {
                folders.insert(*name, opened.create_folder(name).await?.id);
            }
            for sample in fixtures::samples() {
                let draft = vault::Draft {
                    content: sample.content,
                    folder_id: sample.folder.map(|name| folders[name].clone()),
                    collection_ids: Vec::new(),
                };
                let item = opened.create_item(&vault::Scope::Personal, &draft).await?;
                for (name, data) in &sample.attachments {
                    opened
                        .upload_attachment(&vault::Scope::Personal, &item, name, data)
                        .await?;
                }
                if sample.archived {
                    opened
                        .set_archived(std::slice::from_ref(&item.id), true)
                        .await?;
                }
            }
        }
        lines.push(format!("{prefix}_SERVER={server}"));
        lines.push(format!("{prefix}_PASSWORD={password}"));
        lines.push(format!(
            "{prefix}_CLIENT_ID={}",
            account.credentials.client_id
        ));
        lines.push(format!(
            "{prefix}_CLIENT_SECRET={}",
            account.credentials.client_secret
        ));
    }
    lines.push("BACKUP_PASSPHRASE=sandbox-backup-passphrase".to_string());
    println!("{}", lines.join("\n"));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_options_before_a_subcommand_are_rejected() {
        let parse = |args: &[&str]| {
            let cli = Cli::try_parse_from([&["vault-mirror"], args].concat())?;
            check_placement(&cli)?;
            anyhow::Ok(cli)
        };
        assert!(parse(&["--dry-run", "run"]).is_err());
        assert!(parse(&["--allow-mass-delete", "daemon"]).is_err());
        assert!(parse(&["run", "--dry-run"]).unwrap().command.is_some());
        assert!(parse(&["--dry-run"]).unwrap().run.dry_run);
        let file = parse(&["--env-file", "x.env", "check"]).unwrap();
        assert_eq!(file.env_file, Some(PathBuf::from("x.env")));
        assert!(parse(&["check", "--env-file", "x.env"]).is_ok());
    }
}
