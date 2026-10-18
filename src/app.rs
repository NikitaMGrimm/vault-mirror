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
    backup::{BackupOptions, BackupReport, backup},
    config::{Config, VaultConfig},
    mirror::{Endpoint, MirrorOptions, Reporter, mirror},
    plan::{Safety, Summary},
    store::{self, FolderReceipts, Keys, Store},
    text::count,
    vault::{Account, Scope},
};

pub const STATUS_FILE: &str = "last-run.json";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = "run.lock";

#[derive(Debug, Clone, Copy, Default)]
pub struct RunFlags {
    pub allow_mass_delete: bool,
    pub reverse: bool,
    pub allow_overwrite: bool,
}

/// The record of a run, written to `last-run.json`. It holds counts, times
/// and file names, never vault content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Status {
    pub status: RunResult,
    pub started_at: String,
    pub finished_at: String,
    pub duration_seconds: f64,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<SummaryRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_backup: Option<BackupReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_backup: Option<BackupReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunResult {
    Success,
    #[default]
    Failed,
}

impl Status {
    pub fn succeeded(&self) -> bool {
        self.status == RunResult::Success
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SummaryRecord {
    pub changed: bool,
    #[serde(flatten)]
    pub summary: Summary,
}

/// Ids this installation presents to each server. Kept across runs so the
/// servers see one returning device, not a new login every time.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    device_ids: HashMap<String, String>,
}

pub fn device_id(backups_dir: &Path, role: &str) -> Result<String> {
    store::create_private_dir(backups_dir)?;
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
    store::write_private(&path, &serde_json::to_vec_pretty(&state)?)?;
    Ok(id)
}

/// Claims the backup directory for one command; it is released when
/// the file is closed. Two runs at once could both prune and write the same
/// snapshots and attachment files, and would both write `last-run.json`.
pub fn lock_backups_dir(backups_dir: &Path) -> Result<fs::File> {
    store::create_private_dir(backups_dir)?;
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
    let mut options = ClientOptions::default();
    if let Some(path) = &config.ca_cert_file {
        options.extra_root_certs.push(
            fs::read(path).with_context(|| format!("reading CA_CERT_FILE {}", path.display()))?,
        );
    }
    Ok(build_http_client(&options)?)
}

pub fn mirror_options(config: &Config, flags: RunFlags) -> MirrorOptions {
    MirrorOptions {
        safety: Safety {
            allow_mass_delete: flags.allow_mass_delete,
            allow_reverse: flags.reverse,
            allow_overwrite: flags.allow_overwrite,
        },
        filter: config.filter.clone(),
        skip_attachments: config.skip_attachments,
    }
}

pub fn scope_of(vault: &VaultConfig) -> Scope {
    match &vault.organization_id {
        Some(id) => Scope::Organization(id.clone()),
        None => Scope::Personal,
    }
}

pub async fn open_account(
    backups_dir: &Path,
    http: &reqwest::Client,
    vault: &VaultConfig,
    role: &str,
) -> Result<Account> {
    let credentials = ApiCredentials {
        client_id: vault.client_id.clone(),
        client_secret: vault.client_secret.clone(),
        device_id: device_id(backups_dir, role)?,
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
/// a `Status` with `RunResult::Failed`.
pub async fn run_once(config: &Config, flags: RunFlags, reporter: &dyn Reporter) -> Status {
    let started = OffsetDateTime::now_utc();
    let mut progress = Progress {
        stage: "configuration",
        status: Status {
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
        Ok(()) => status.status = RunResult::Success,
        Err(error) => {
            status.status = RunResult::Failed;
            status.failure_stage = Some(progress.stage.to_string());
            let message: String = format!("{error:#}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            status.error = Some(message.chars().take(1000).collect());
        }
    }
    // A run turned away by the lock leaves the status file and the
    // notification to the run that holds it.
    if lock.is_none() {
        return status;
    }
    if let Err(error) = write_status(&config.backups_dir, &status) {
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
    let http = http_client(config)?;
    let keys = Keys::open(&config.backups_dir, &config.backup.passphrase)?;
    let backup_options = BackupOptions {
        retention_days: config.backup.retention_days,
        now,
    };

    progress.stage = "source login";
    let source = open_account(&config.backups_dir, &http, &config.source, "source").await?;
    let source_scope = scope_of(&config.source);
    progress.stage = "source backup";
    reporter.status("Backing up", "the source vault");
    let store = Store::new(&config.backups_dir, "source", &keys);
    let (report, source_read) =
        backup(&source, &source_scope, &store, &backup_options, reporter).await?;
    progress.status.source_backup = Some(report);

    let Some(dest_config) = &config.dest else {
        return Ok(());
    };
    progress.stage = "destination login";
    let dest = open_account(&config.backups_dir, &http, dest_config, "dest").await?;
    let dest_scope = scope_of(dest_config);
    // Attachment content is stored on this side too. A destination file can
    // be the only copy: one the source never had, one changed in the
    // destination, or one in a vault this tool did not fill.
    progress.stage = "destination backup";
    reporter.status("Backing up", "the destination vault");
    let store = Store::new(&config.backups_dir, "dest", &keys);
    let (report, dest_read) = backup(&dest, &dest_scope, &store, &backup_options, reporter).await?;
    progress.status.dest_backup = Some(report);

    let source = Endpoint {
        account: &source,
        scope: source_scope,
    };
    let dest = Endpoint {
        account: &dest,
        scope: dest_scope,
    };
    let options = mirror_options(config, flags);
    progress.stage = "mirror";
    let folders = FolderReceipts::new(&config.backups_dir);
    let mirrored = mirror(
        &source,
        &dest,
        &source_read,
        &dest_read,
        &options,
        &folders,
        reporter,
    )
    .await;
    match mirrored {
        Ok(outcome) => {
            progress.status.mirror = Some(SummaryRecord {
                changed: outcome.changed,
                summary: outcome.summary,
            });
            progress.status.warnings.extend(outcome.warnings);
            Ok(())
        }
        Err(error) => {
            progress.stage = error.stage;
            progress.status.mirror = error.summary.map(|summary| SummaryRecord {
                changed: false,
                summary,
            });
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
    store::write_private(&status_path(backups_dir), &data)
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
        "vault-mirror {}",
        if status.succeeded() {
            "succeeded"
        } else {
            "FAILED"
        }
    )];
    if let Some(SummaryRecord {
        summary: mirror, ..
    }) = &status.mirror
    {
        parts.push(format!(
            "{} created, {} updated, {} deleted, {} unchanged, {} deleted",
            count(mirror.items_created, "item"),
            mirror.items_updated,
            mirror.items_deleted,
            mirror.items_unchanged,
            count(mirror.folders_deleted, "folder")
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
        parts.push(count(status.warnings.len(), "warning"));
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
        .await;
    match sent {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => reporter.warn(&format!(
            "Could not send the notification: the server answered {}",
            response.status()
        )),
        // The URL can embed a token, so it is not repeated here.
        Err(error) => reporter.warn(&format!(
            "Could not send the notification: {}",
            error.without_url()
        )),
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
            "the last successful run finished {} ago",
            count(age.whole_hours().unsigned_abs() as usize, "hour")
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
            status: RunResult::Success,
            finished_at: "2026-10-14T02:00:00Z".into(),
            mirror: Some(SummaryRecord {
                changed: false,
                summary: Summary {
                    items_created: 2,
                    items_unchanged: 40,
                    ..Summary::default()
                },
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
        fn status(&self, _: &str, _: &str) {}
        fn warn(&self, _: &str) {}
    }

    #[tokio::test]
    async fn a_run_is_turned_away_while_another_uses_the_backup_directory() {
        let dir = tempfile::tempdir().unwrap();
        let notified = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        notified.set_nonblocking(true).unwrap();
        let notify_url = format!("http://{}/", notified.local_addr().unwrap());
        let env: HashMap<String, String> = [
            ("NOTIFY_FAILURE_URL", notify_url.as_str()),
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
        // The status file and the notification are the running run's.
        assert!(!status_path(dir.path()).exists());
        assert!(notified.accept().is_err(), "the turned-away run notified");
        drop(notified);
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
            status: RunResult::Failed,
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
            "vault-mirror succeeded; 2 items created, 0 updated, 0 deleted, 40 unchanged, 0 \
             folders deleted"
        );
        let failed = Status {
            status: RunResult::Failed,
            mirror: None,
            failure_stage: Some("planning".into()),
            error: Some("refused".into()),
            ..success()
        };
        assert_eq!(
            describe(&failed),
            "vault-mirror FAILED; during planning: refused"
        );
    }
}
