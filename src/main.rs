use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

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
};

/// Mirror and back up a Bitwarden or Vaultwarden vault.
///
/// Configuration comes from environment variables; see the README. Without a
/// subcommand, one run is performed.
#[derive(Parser)]
// Run options belong after `run`, or stand alone without a subcommand. Before
// a subcommand they would be ignored, so they are rejected there.
#[command(
    name = "vault-mirror",
    version,
    max_term_width = 100,
    args_conflicts_with_subcommands = true
)]
struct Cli {
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
    /// Let this run delete more of the destination than MAX_DELETE_PERCENT.
    #[arg(long)]
    allow_mass_delete: bool,
}

#[derive(Subcommand)]
enum Command {
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

struct Console;

impl Console {
    fn line(level: &str, message: &str) {
        // People at a terminal know the time; logs need it.
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            match level {
                "INFO" => println!("{message}"),
                _ => println!("{}: {message}", level.to_lowercase()),
            }
            return;
        }
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
    fn info(&self, message: &str) {
        Self::line("INFO", message);
    }
    fn warn(&self, message: &str) {
        Self::line("WARN", message);
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
            eprintln!("Error: could not start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn config() -> Result<Config> {
    let env: HashMap<String, String> = std::env::vars().collect();
    Config::from_env(&env).map_err(|error| anyhow!("configuration: {error}"))
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
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
        Console.info(&app::describe(&status));
    } else {
        Console::line("ERROR", &app::describe(&status));
    }
    status.succeeded()
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
        Console.info(&format!(
            "Next run at {}.",
            next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ));
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
    Console.info("Stopping.");
    Ok(ExitCode::SUCCESS)
}

async fn check() -> Result<ExitCode> {
    let config = config()?;
    let http = app::http_client(&config)?;
    let source = app::open_account(&config, &http, &config.source, "source").await?;
    let source_scope = app::scope_of(&config.source);
    let describe = |vault: &vault_mirror::vault::Vault| {
        let live: Vec<_> = vault.items.iter().filter(|item| !item.in_trash).collect();
        format!(
            "{} item(s), {} folder(s), {} collection(s), {} attachment(s)",
            live.len(),
            vault.folders.len(),
            vault.collections.len(),
            live.iter()
                .map(|item| item.attachments.len())
                .sum::<usize>()
        )
    };
    Console.info(&format!(
        "Source login works: {}.",
        describe(&source.load(&source_scope).await?)
    ));
    let Some(dest_config) = &config.dest else {
        Console.info("No destination is configured; runs only back up the source.");
        return Ok(ExitCode::SUCCESS);
    };
    let dest = app::open_account(&config, &http, dest_config, "dest").await?;
    let dest_scope = app::scope_of(dest_config);
    Console.info(&format!(
        "Destination login works: {}.",
        describe(&dest.load(&dest_scope).await?)
    ));

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
    Ok(ExitCode::SUCCESS)
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
                "{} item(s), {} attachment file(s)",
                meta.items,
                meta.attachments.len()
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
    Console.info(&format!(
        "Restoring {} item(s) from {}.",
        restorable.items.len(),
        path.display()
    ));

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
    Console.info(&format!(
        "Restored {} item(s) and {} attachment(s); created {} folder(s) and {} collection(s).",
        report.items,
        report.attachments_restored,
        report.folders_created,
        report.collections_created
    ));
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
    println!(
        "Wrote {} item(s) and {written} attachment(s) to {}, unencrypted. export.json can be imported by any Bitwarden client.",
        restorable.items.len(),
        output.display()
    );
    if missing > 0 {
        println!(
            "{missing} attachment(s) are listed in the snapshot but their content is not in the backup."
        );
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
        let parse = |args: &[&str]| Cli::try_parse_from([&["vault-mirror"], args].concat());
        assert!(parse(&["--dry-run", "run"]).is_err());
        assert!(parse(&["--allow-mass-delete", "daemon"]).is_err());
        assert!(parse(&["run", "--dry-run"]).unwrap().command.is_some());
        assert!(parse(&["--dry-run"]).unwrap().run.dry_run);
    }
}
