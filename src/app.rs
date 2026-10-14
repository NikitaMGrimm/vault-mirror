//! One complete run: back up the source, back up the destination, mirror.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    api::{ApiCredentials, ClientOptions, ServerUrls, build_http_client},
    backup::{BackupOptions, BackupReport, backup, backup_loaded},
    config::{Config, VaultConfig},
    mirror::{Endpoint, MirrorOptions, Reporter, mirror_onto},
    plan::{Safety, Summary},
    store::{Keys, Store},
    vault::{Account, Scope},
};

pub const STATUS_FILE: &str = "last-run.json";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = "run.lock";

#[derive(Debug, Clone, Copy, Default)]
pub struct RunFlags {
    pub dry_run: bool,
    pub allow_initial_overwrite: bool,
    pub allow_mass_delete: bool,
}

/// The record of a run, written to `last-run.json`. It holds counts, times
/// and file names, never vault content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Status {
    pub status: String,
    pub dry_run: bool,
    pub started_at: String,
    pub finished_at: String,
    pub duration_seconds: f64,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<SummaryRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_backup: Option<BackupRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_backup: Option<BackupRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Status {
    pub fn succeeded(&self) -> bool {
        self.status == "success"
    }
}

// The status file is read back by `healthcheck`, so these mirror the report
// types with owned, deserializable fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SummaryRecord {
    pub changed: bool,
    pub items_created: usize,
    pub items_updated: usize,
    pub items_deleted: usize,
    pub items_unchanged: usize,
    pub folders_created: usize,
    pub folders_deleted: usize,
    pub collections_created: usize,
    pub collections_deleted: usize,
    pub attachments_copied: usize,
}

impl SummaryRecord {
    fn new(summary: Summary, changed: bool) -> Self {
        Self {
            changed,
            items_created: summary.items_created,
            items_updated: summary.items_updated,
            items_deleted: summary.items_deleted,
            items_unchanged: summary.items_unchanged,
            folders_created: summary.folders_created,
            folders_deleted: summary.folders_deleted,
            collections_created: summary.collections_created,
            collections_deleted: summary.collections_deleted,
            attachments_copied: summary.attachments_copied,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BackupRecord {
    pub snapshot: Option<String>,
    pub items: usize,
    pub attachments_downloaded: usize,
    pub snapshots_pruned: usize,
    pub attachments_pruned: usize,
}

impl From<BackupReport> for BackupRecord {
    fn from(report: BackupReport) -> Self {
        Self {
            snapshot: report.snapshot,
            items: report.items,
            attachments_downloaded: report.attachments_downloaded,
            snapshots_pruned: report.pruned.snapshots_removed,
            attachments_pruned: report.pruned.attachments_removed,
        }
    }
}

/// Ids this installation presents to each server. Kept across runs so the
/// servers see one returning device, not a new login every time.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    device_ids: HashMap<String, String>,
}

pub fn device_id(backups_dir: &Path, role: &str) -> Result<String> {
    fs::create_dir_all(backups_dir)
        .with_context(|| format!("creating {}", backups_dir.display()))?;
    let path = backups_dir.join(STATE_FILE);
    let mut state: State = fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    if let Some(id) = state.device_ids.get(role) {
        return Ok(id.clone());
    }
    let id = uuid::Uuid::new_v4().to_string();
    state.device_ids.insert(role.to_string(), id.clone());
    write_atomically(&path, &serde_json::to_vec_pretty(&state)?)?;
    Ok(id)
}

fn write_atomically(path: &Path, data: &[u8]) -> Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(directory)?;
    std::io::Write::write_all(&mut temp, data)?;
    temp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Claims the backup directory for one run or restore; it is released when
/// the file is closed. Two runs at once could both prune and write the same
/// snapshots and attachment files, and would both write `last-run.json`.
pub fn lock_backups_dir(backups_dir: &Path) -> Result<fs::File> {
    fs::create_dir_all(backups_dir)
        .with_context(|| format!("creating {}", backups_dir.display()))?;
    let path = backups_dir.join(LOCK_FILE);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(anyhow!(
            "another vault-mirror run is using BACKUPS_DIR {}; try again when it has finished",
            backups_dir.display()
        )),
        Err(fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking {}", path.display()))
        }
    }
}

pub fn http_client(config: &Config) -> Result<reqwest::Client> {
    let mut options = ClientOptions {
        client_version: config.client_version.clone(),
        ..ClientOptions::default()
    };
    if let Some(path) = &config.ca_cert_file {
        options.extra_root_certs.push(
            fs::read(path).with_context(|| format!("reading CA_CERT_FILE {}", path.display()))?,
        );
    }
    Ok(build_http_client(&options)?)
}

pub fn scope_of(vault: &VaultConfig) -> Scope {
    match &vault.organization_id {
        Some(id) => Scope::Organization(id.clone()),
        None => Scope::Personal,
    }
}

pub async fn open_account(
    config: &Config,
    http: &reqwest::Client,
    vault: &VaultConfig,
    role: &str,
) -> Result<Account> {
    let credentials = ApiCredentials {
        client_id: vault.client_id.clone(),
        client_secret: vault.client_secret.clone(),
        device_id: device_id(&config.backups_dir, role)?,
    };
    let urls = ServerUrls::from_server(&vault.server)?;
    Account::open(http.clone(), urls, credentials, &vault.password)
        .await
        .with_context(|| format!("opening the {role} vault"))
}

struct Progress {
    stage: &'static str,
    status: Status,
}

/// Runs once and records the result. Never returns an error: a failed run is
/// a `Status` with `status == "failed"`.
pub async fn run_once(config: &Config, flags: RunFlags, reporter: &dyn Reporter) -> Status {
    let started = OffsetDateTime::now_utc();
    let mut progress = Progress {
        stage: "configuration",
        status: Status {
            dry_run: flags.dry_run,
            version: env!("CARGO_PKG_VERSION").to_string(),
            ..Status::default()
        },
    };
    let (lock, result) = match lock_backups_dir(&config.backups_dir) {
        Ok(lock) => (
            Some(lock),
            execute(config, flags, reporter, &mut progress, started).await,
        ),
        Err(error) => (None, Err(error)),
    };

    let finished = OffsetDateTime::now_utc();
    let mut status = progress.status;
    status.started_at = started.format(&Rfc3339).unwrap_or_default();
    status.finished_at = finished.format(&Rfc3339).unwrap_or_default();
    status.duration_seconds = ((finished - started).as_seconds_f64() * 1000.0).round() / 1000.0;
    match result {
        Ok(()) => status.status = "success".into(),
        Err(error) => {
            status.status = "failed".into();
            status.failure_stage = Some(progress.stage.to_string());
            let message: String = format!("{error:#}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            status.error = Some(message.chars().take(1000).collect());
        }
    }
    // A run turned away by the lock leaves the status file to the run that
    // holds it.
    if lock.is_some()
        && let Err(error) = write_status(&config.backups_dir, &status)
    {
        reporter.warn(&format!("Could not write the status file: {error:#}"));
    }
    drop(lock);
    notify(config, &status, reporter).await;
    status
}

async fn execute(
    config: &Config,
    flags: RunFlags,
    reporter: &dyn Reporter,
    progress: &mut Progress,
    now: OffsetDateTime,
) -> Result<()> {
    if !config.ignored.is_empty() {
        reporter.info(&format!(
            "These settings are no longer used and are ignored: {}.",
            config.ignored.join(", ")
        ));
    }
    let http = http_client(config)?;
    let keys = match &config.backup {
        Some(backup) => Some(Keys::open(&config.backups_dir, &backup.protection)?),
        None => None,
    };
    let backup_options = config.backup.as_ref().map(|backup| BackupOptions {
        attachments: backup.attachments,
        retention: backup.retention.as_ref(),
        now,
    });

    progress.stage = "source login";
    let source = open_account(config, &http, &config.source, "source").await?;
    let source_scope = scope_of(&config.source);
    if let (Some(keys), Some(options)) = (&keys, &backup_options) {
        progress.stage = "source backup";
        reporter.info("Backing up the source vault.");
        let store = Store::open(&config.backups_dir, "source", keys)?;
        progress.status.source_backup = Some(
            backup(&source, &source_scope, &store, options, reporter)
                .await?
                .into(),
        );
    }

    let Some(dest_config) = &config.dest else {
        return Ok(());
    };
    progress.stage = "destination login";
    let dest = open_account(config, &http, dest_config, "dest").await?;
    let dest_scope = scope_of(dest_config);
    // Attachment content is stored on this side too. A destination file can
    // be the only copy: one the source never had, one changed in the
    // destination, or one in a vault this tool did not fill. The mirror is
    // about to delete or replace such files.
    let mut dest_vault = None;
    if let (Some(keys), Some(options)) = (&keys, &backup_options) {
        progress.stage = "destination backup";
        reporter.info("Backing up the destination vault before changing it.");
        let store = Store::open(&config.backups_dir, "dest", keys)?;
        let vault = dest.load(&dest_scope).await.context("reading the vault")?;
        progress.status.dest_backup = Some(
            backup_loaded(&dest, &dest_scope, &vault, &store, options, reporter)
                .await?
                .into(),
        );
        // The mirror changes only what this backup holds.
        dest_vault = Some(vault);
    }

    progress.stage = "mirror";
    let options = MirrorOptions {
        dry_run: flags.dry_run,
        safety: Safety {
            allow_initial_overwrite: flags.allow_initial_overwrite,
            allow_mass_delete: flags.allow_mass_delete,
            ..config.safety.clone()
        },
        concurrency: config.concurrency,
        verify_attachments: config.verify_attachments,
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
    match mirror_onto(&source, &dest, dest_vault, &options, reporter).await {
        Ok(outcome) => {
            progress.status.mirror = Some(SummaryRecord::new(outcome.summary, outcome.changed));
            progress.status.warnings = outcome.warnings;
            Ok(())
        }
        Err(error) => {
            progress.stage = error.stage;
            progress.status.mirror = error
                .summary
                .map(|summary| SummaryRecord::new(summary, false));
            Err(error.source)
        }
    }
}

pub fn status_path(backups_dir: &Path) -> PathBuf {
    backups_dir.join(STATUS_FILE)
}

fn write_status(backups_dir: &Path, status: &Status) -> Result<()> {
    fs::create_dir_all(backups_dir)?;
    let mut data = serde_json::to_vec_pretty(status)?;
    data.push(b'\n');
    write_atomically(&status_path(backups_dir), &data)
}

pub fn read_status(backups_dir: &Path) -> Result<Status> {
    let path = status_path(backups_dir);
    let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("{} is not a status file", path.display()))
}

/// One line describing a run, safe to send to a notification service.
pub fn describe(status: &Status) -> String {
    let mut parts = vec![format!(
        "vault-mirror {}{}",
        if status.succeeded() {
            "succeeded"
        } else {
            "FAILED"
        },
        if status.dry_run { " (dry run)" } else { "" }
    )];
    if let Some(mirror) = &status.mirror {
        parts.push(format!(
            "{} items created, {} updated, {} deleted, {} unchanged",
            mirror.items_created,
            mirror.items_updated,
            mirror.items_deleted,
            mirror.items_unchanged
        ));
    }
    if let Some(snapshot) = status
        .source_backup
        .as_ref()
        .and_then(|backup| backup.snapshot.as_ref())
    {
        parts.push(format!("snapshot {snapshot}"));
    }
    if let (Some(stage), Some(error)) = (&status.failure_stage, &status.error) {
        parts.push(format!("during {stage}: {error}"));
    }
    if !status.warnings.is_empty() {
        parts.push(format!("{} warning(s)", status.warnings.len()));
    }
    parts.join("; ")
}

async fn notify(config: &Config, status: &Status, reporter: &dyn Reporter) {
    let url = if status.succeeded() {
        &config.notify_success_url
    } else {
        &config.notify_failure_url
    };
    let Some(url) = url else { return };
    let Ok(http) = http_client(config) else {
        return;
    };
    let sent = http
        .post(url)
        .timeout(Duration::from_secs(20))
        .header("content-type", "text/plain; charset=utf-8")
        .body(describe(status))
        .send()
        .await
        .and_then(|response| response.error_for_status());
    if let Err(error) = sent {
        // The URL can embed a token, so it is not repeated here.
        reporter.warn(&format!(
            "Could not send the notification: {}",
            error.without_url()
        ));
    }
}

/// Decides whether the last recorded run is recent and successful.
pub fn health(
    status: &Status,
    max_age: Duration,
    now: OffsetDateTime,
) -> std::result::Result<(), String> {
    if !status.succeeded() {
        return Err(format!(
            "the last run failed during {}: {}",
            status
                .failure_stage
                .as_deref()
                .unwrap_or("an unknown stage"),
            status.error.as_deref().unwrap_or("no error recorded")
        ));
    }
    let finished = OffsetDateTime::parse(&status.finished_at, &Rfc3339)
        .map_err(|_| "the status file has no valid finish time".to_string())?;
    let age = now - finished;
    if age > max_age {
        return Err(format!(
            "the last successful run finished {} hour(s) ago",
            age.whole_hours()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn success() -> Status {
        Status {
            status: "success".into(),
            finished_at: "2026-10-14T02:00:00Z".into(),
            mirror: Some(SummaryRecord {
                items_created: 2,
                items_unchanged: 40,
                ..SummaryRecord::default()
            }),
            ..Status::default()
        }
    }

    #[test]
    fn device_ids_are_stable_per_role() {
        let dir = tempfile::tempdir().unwrap();
        let source = device_id(dir.path(), "source").unwrap();
        let dest = device_id(dir.path(), "dest").unwrap();
        assert_ne!(source, dest);
        assert_eq!(device_id(dir.path(), "source").unwrap(), source);
        assert_eq!(device_id(dir.path(), "dest").unwrap(), dest);
        assert!(uuid::Uuid::parse_str(&source).is_ok());
    }

    struct Quiet;

    impl Reporter for Quiet {
        fn info(&self, _: &str) {}
        fn warn(&self, _: &str) {}
    }

    #[tokio::test]
    async fn a_run_is_turned_away_while_another_uses_the_backup_directory() {
        let dir = tempfile::tempdir().unwrap();
        let env: HashMap<String, String> = [
            ("SOURCE_SERVER", "http://127.0.0.1:9"),
            ("SOURCE_CLIENT_ID", "user.source"),
            ("SOURCE_CLIENT_SECRET", "source-secret"),
            ("SOURCE_PASSWORD", "source password"),
            ("BACKUP_PASSPHRASE", "backup passphrase"),
            ("BACKUPS_DIR", dir.path().to_str().unwrap()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        let config = Config::from_env(&env).unwrap();
        let running = lock_backups_dir(dir.path()).unwrap();

        let refused = run_once(&config, RunFlags::default(), &Quiet).await;
        assert!(!refused.succeeded());
        let error = refused.error.unwrap();
        assert!(
            error.contains("another vault-mirror run is using BACKUPS_DIR"),
            "{error}"
        );
        // The status file is the running run's to write.
        assert!(!status_path(dir.path()).exists());
        assert!(!dir.path().join("key.age").exists());

        drop(running);
        lock_backups_dir(dir.path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let lock = fs::metadata(dir.path().join(LOCK_FILE)).unwrap();
            assert_eq!(lock.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_status_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let status = Status {
            warnings: vec!["w".into()],
            ..success()
        };
        write_status(dir.path(), &status).unwrap();
        assert_eq!(read_status(dir.path()).unwrap(), status);
        assert!(read_status(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn health_requires_a_recent_success() {
        let day = Duration::from_secs(26 * 3600);
        assert_eq!(
            health(&success(), day, datetime!(2026-10-14 20:00:00 UTC)),
            Ok(())
        );
        let stale = health(&success(), day, datetime!(2026-10-16 02:00:00 UTC)).unwrap_err();
        assert!(stale.contains("48 hour"), "{stale}");
        let failed = Status {
            status: "failed".into(),
            failure_stage: Some("source login".into()),
            error: Some("bad key".into()),
            ..success()
        };
        let message = health(&failed, day, datetime!(2026-10-14 02:01:00 UTC)).unwrap_err();
        assert!(
            message.contains("source login") && message.contains("bad key"),
            "{message}"
        );
    }

    #[test]
    fn descriptions_are_one_line_summaries() {
        assert_eq!(
            describe(&success()),
            "vault-mirror succeeded; 2 items created, 0 updated, 0 deleted, 40 unchanged"
        );
        let failed = Status {
            status: "failed".into(),
            dry_run: true,
            mirror: None,
            failure_stage: Some("planning".into()),
            error: Some("refused".into()),
            ..success()
        };
        assert_eq!(
            describe(&failed),
            "vault-mirror FAILED (dry run); during planning: refused"
        );
    }
}
