//! One complete run: back up the source, back up the destination, mirror.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    api::{self, ApiCredentials, ClientOptions, ServerUrls, build_http_client},
    backup::{BackupOptions, BackupReport, backup_loaded},
    config::{Config, VaultConfig},
    mirror::{Endpoint, MirrorOptions, Reporter, mirror_after_pull, mirror_onto, pull, unmark},
    plan::{Safety, Summary, marker_of},
    store::{self, Keys, Store},
    text::count,
    vault::{Account, Scope},
};

pub const STATUS_FILE: &str = "last-run.json";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = "run.lock";
const RESTORE_HOLD_FILE: &str = "restore-hold.json";
const PULL_HOLD_FILE: &str = "pull-cleanup-hold.json";

#[derive(Debug, Clone, Copy, Default)]
pub struct RunFlags {
    pub dry_run: bool,
    pub allow_mass_delete: bool,
    pub reverse: bool,
    pub pull: bool,
}

/// The record of a run, written to `last-run.json`. It holds counts, times
/// and file names, never vault content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Status {
    pub status: RunResult,
    pub dry_run: bool,
    pub started_at: String,
    pub finished_at: String,
    pub duration_seconds: f64,
    pub version: String,
    /// What a pull changed in the source, before the mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulled: Option<SummaryRecord>,
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

// The status file is read back by `healthcheck`, so this mirrors the report
// with owned, deserializable fields.
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

/// Claims the backup directory for one run or restore; it is released when
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

/// Pauses mirroring after restoring independent items. The caller must hold
/// the backup directory lock and persist this before the first vault write.
pub fn hold_after_restore(backups_dir: &Path) -> Result<()> {
    let path = backups_dir.join(RESTORE_HOLD_FILE);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&path)?.file_type().is_file() {
                return Err(anyhow!(
                    "recovery hold is not a regular file: {}",
                    path.display()
                ));
            }
            let file = fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            file.sync_all()?;
            #[cfg(unix)]
            fs::File::open(backups_dir)?.sync_all()?;
            return Ok(());
        }
        Err(error) => return Err(error).with_context(|| format!("creating {}", path.display())),
    };
    file.write_all(b"{\"format\":1}\n")?;
    file.sync_all()?;
    #[cfg(unix)]
    fs::File::open(backups_dir)?.sync_all()?;
    Ok(())
}

fn check_recovery_holds(backups_dir: &Path) -> Result<()> {
    for (file, reason) in [
        (RESTORE_HOLD_FILE, "restored items are independent"),
        (
            PULL_HOLD_FILE,
            "a pull was interrupted or could not be verified",
        ),
    ] {
        let path = backups_dir.join(file);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("checking {}", path.display()));
            }
            Ok(_) => {
                return Err(anyhow!(
                    "mirroring is paused by {}; {reason}. Stop scheduling and follow the recovery reconciliation steps in docs/backups.md before removing this hold",
                    path.display()
                ));
            }
        }
    }
    Ok(())
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

/// The warning for a vault whose server address sends the API key and access
/// tokens across the internet unencrypted, if it does. `role` is `source` or
/// `dest`.
pub fn plain_http_warning(vault: &VaultConfig, role: &str) -> Option<String> {
    // The address the login actually goes to: Bitwarden's cloud is always
    // reached over https, whatever was written.
    let urls = ServerUrls::from_server(&vault.server).ok()?;
    api::is_plain_http_to_remote(&urls.identity).then(|| {
        format!(
            "{}_SERVER uses http:// for a host outside your network, so the API key crosses \
             the internet unencrypted. Use https:// instead.",
            role.to_uppercase()
        )
    })
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
    // A run turned away by the lock leaves the status file to the run that
    // holds it.
    if lock.is_some()
        && !status.dry_run
        && let Err(error) = write_status(&config.backups_dir, &status)
    {
        reporter.warn(&format!("Could not write the status file: {error:#}"));
    }
    drop(lock);
    if !status.dry_run {
        notify(config, &status, reporter).await;
    }
    status
}

async fn execute(
    config: &Config,
    flags: RunFlags,
    reporter: &dyn Reporter,
    progress: &mut Progress,
    now: OffsetDateTime,
) -> Result<()> {
    if config.dest.is_some() {
        progress.stage = "recovery hold";
        check_recovery_holds(&config.backups_dir)?;
    }
    progress.stage = "backup metadata";
    store::migrate_metadata(&config.backups_dir)?;
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

    for warning in config
        .vaults()
        .filter_map(|(vault, role)| plain_http_warning(vault, role))
    {
        reporter.warn(&warning);
        progress.status.warnings.push(warning);
    }
    progress.stage = "source login";
    let source = open_account(&config.backups_dir, &http, &config.source, "source").await?;
    let source_scope = scope_of(&config.source);
    // A pull writes to the source, and changes only what this backup holds.
    let mut source_vault = None;
    if let (Some(keys), Some(options)) = (&keys, &backup_options) {
        progress.stage = "source backup";
        reporter.status("Backing up", "the source vault");
        let store = Store::open(&config.backups_dir, "source", keys)?;
        let vault = source
            .load(&source_scope)
            .await
            .context("reading the vault")?;
        progress.status.source_backup = Some(
            backup_loaded(&source, &source_scope, &vault, &store, options, reporter)
                .await?
                .into(),
        );
        source_vault = Some(vault);
    }

    let Some(dest_config) = &config.dest else {
        return Ok(());
    };
    progress.stage = "destination login";
    let dest = open_account(&config.backups_dir, &http, dest_config, "dest").await?;
    let dest_scope = scope_of(dest_config);
    // Attachment content is stored on this side too. A destination file can
    // be the only copy: one the source never had, one changed in the
    // destination, or one in a vault this tool did not fill. The mirror is
    // about to delete or replace such files.
    let mut dest_vault = None;
    if let (Some(keys), Some(options)) = (&keys, &backup_options) {
        progress.stage = "destination backup";
        reporter.status("Backing up", "the destination vault");
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

    let source = Endpoint {
        account: &source,
        scope: source_scope,
    };
    let dest = Endpoint {
        account: &dest,
        scope: dest_scope,
    };
    let options = MirrorOptions {
        dry_run: flags.dry_run,
        safety: Safety {
            allow_mass_delete: flags.allow_mass_delete,
            allow_reverse: flags.reverse || flags.pull,
            ..config.safety.clone()
        },
        concurrency: config.concurrency,
        verify_attachments: config.verify_attachments,
        filter: config.filter.clone(),
        dest_backed_up: dest_vault.is_some(),
    };

    let mut cleanup_source_ids = Default::default();
    let pull_hold = config.backups_dir.join(PULL_HOLD_FILE);
    if flags.pull {
        progress.stage = "pull";
        let backed_up = source_vault.is_some();
        let source_vault = match source_vault {
            Some(vault) => vault,
            None => source.account.load(&source.scope).await?,
        };
        let dest_vault = match &dest_vault {
            Some(vault) => vault.clone(),
            None => dest.account.load(&dest.scope).await?,
        };
        // A pull that fails after a source write can leave the source holding
        // only part of an item. The next pull would take that for an edit made
        // in the source and copy the loss into the destination, so the hold
        // is taken before the first write and kept until the whole run has
        // been verified.
        if !flags.dry_run {
            store::write_private_new(&pull_hold, b"{\"format\":1}\n")
                .with_context(|| format!("persisting {} before the pull", pull_hold.display()))?;
            // A pending marker no hold covers was left by an older version or
            // by a direct caller of `pull`.
            if let Some(item) = source_vault.items.iter().find(|item| {
                !item.in_trash && marker_of(item).is_some_and(|marker| marker.files_pending())
            }) {
                bail!(
                    "source item {} was left unfinished by a pull that no hold covers; mirroring is paused by {}. Reconcile the vaults using docs/backups.md before removing this hold",
                    item.id,
                    pull_hold.display()
                );
            }
        }
        let pulled = pull(
            &source,
            &dest,
            &source_vault,
            &dest_vault,
            &options,
            backed_up,
            reporter,
        )
        .await;
        let pulled = match flags.dry_run {
            true => pulled?,
            false => pulled.with_context(|| format!("mirroring is paused by {}; reconcile the vaults using docs/backups.md before removing this hold", pull_hold.display()))?,
        };
        progress.status.pulled = Some(SummaryRecord {
            changed: !flags.dry_run && pulled.summary != Summary::default(),
            summary: pulled.summary,
        });
        progress.status.warnings.extend(pulled.warnings);
        cleanup_source_ids = pulled.cleanup_source_ids;
        if flags.dry_run {
            return Ok(());
        }
    }

    progress.stage = "mirror";
    let outcome = if flags.pull {
        mirror_after_pull(
            &source,
            &dest,
            dest_vault,
            &options,
            reporter,
            &cleanup_source_ids,
        )
        .await
    } else {
        mirror_onto(&source, &dest, dest_vault, &options, reporter).await
    };
    let cleanup_source_items;
    match outcome {
        Ok(outcome) => {
            cleanup_source_items = outcome.cleanup_source_items;
            progress.status.mirror = Some(SummaryRecord {
                changed: outcome.changed,
                summary: outcome.summary,
            });
            progress.status.warnings.extend(outcome.warnings);
        }
        Err(error) => {
            progress.stage = error.stage;
            progress.status.mirror = error.summary.map(|summary| SummaryRecord {
                changed: false,
                summary,
            });
            return Err(error.source);
        }
    }

    if flags.pull && !cleanup_source_items.is_empty() {
        progress.stage = "removing markers from the source";
        let unmarked = unmark(&source, &dest, &cleanup_source_items, config.concurrency).await
            .with_context(|| format!("mirroring remains paused by {}; reconcile the vaults using docs/backups.md before removing this hold", pull_hold.display()))?;
        if unmarked > 0 {
            reporter.status("Unmarked", &count(unmarked, "source item"));
        }
    }
    if flags.pull {
        fs::remove_file(&pull_hold).with_context(|| format!("removing {}", pull_hold.display()))?;
        store::sync_dir(&config.backups_dir)?;
    }
    Ok(())
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
        "vault-mirror {}{}",
        if status.succeeded() {
            "succeeded"
        } else {
            "FAILED"
        },
        if status.dry_run { " (dry run)" } else { "" }
    )];
    if let Some(SummaryRecord {
        summary: mirror, ..
    }) = &status.mirror
    {
        parts.push(format!(
            "{} created, {} updated, {} deleted, {} unchanged",
            count(mirror.items_created, "item"),
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
    if status.dry_run {
        return Err("the last recorded run was a dry run; complete a real run first".to_string());
    }
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
    async fn an_unencrypted_server_address_is_recorded_as_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let env: HashMap<String, String> = [
            // `.invalid` never resolves, so the login fails without a network.
            ("SOURCE_SERVER", "http://vault.example.invalid"),
            ("SOURCE_CLIENT_ID", "user.source"),
            ("SOURCE_CLIENT_SECRET", "source-secret"),
            ("SOURCE_PASSWORD", "source password"),
            ("BACKUP_PASSPHRASE", "backup passphrase"),
            ("BACKUPS_DIR", dir.path().to_str().unwrap()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let config = Config::from_env(&env).unwrap();

        let status = run_once(&config, RunFlags::default(), &Quiet).await;
        assert_eq!(status.failure_stage.as_deref(), Some("source login"));
        assert_eq!(status.warnings.len(), 1);
        assert!(status.warnings[0].starts_with("SOURCE_SERVER uses http://"));
    }

    #[test]
    fn bitwarden_cloud_is_reached_over_https_whatever_was_written() {
        let vault = |server: &str| VaultConfig {
            server: server.into(),
            client_id: String::new(),
            client_secret: String::new(),
            password: String::new(),
            organization_id: None,
        };
        assert_eq!(
            plain_http_warning(&vault("http://vault.bitwarden.com"), "source"),
            None
        );
        assert!(plain_http_warning(&vault("http://vault.example.com"), "dest").is_some());
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
    fn a_successful_dry_run_does_not_prove_health() {
        let status = Status {
            dry_run: true,
            ..success()
        };
        assert!(
            health(
                &status,
                Duration::from_secs(26 * 3600),
                datetime!(2026-10-14 20:00:00 UTC)
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_dry_run_preserves_the_last_actual_run_status() {
        let dir = tempfile::tempdir().unwrap();
        let env = [
            ("SOURCE_SERVER", "invalid-server-url"),
            ("SOURCE_CLIENT_ID", "user.test"),
            ("SOURCE_CLIENT_SECRET", "synthetic-secret"),
            ("SOURCE_PASSWORD", "synthetic-password"),
            ("DEST_SERVER", "invalid-server-url"),
            ("DEST_CLIENT_ID", "user.dest"),
            ("DEST_CLIENT_SECRET", "synthetic-secret"),
            ("DEST_PASSWORD", "synthetic-password"),
            ("BACKUP_ENABLED", "false"),
            ("BACKUPS_DIR", dir.path().to_str().unwrap()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let config = Config::from_env(&env).unwrap();
        write_status(dir.path(), &success()).unwrap();
        let flags = RunFlags {
            dry_run: true,
            ..RunFlags::default()
        };
        let status = run_once(&config, flags, &Quiet).await;
        assert!(status.dry_run && !status.succeeded());
        assert_eq!(read_status(dir.path()).unwrap(), success());
        fs::remove_file(status_path(dir.path())).unwrap();
        run_once(&config, flags, &Quiet).await;
        assert!(!status_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn recovery_holds_stop_mirroring_before_login_even_with_overrides() {
        for name in [RESTORE_HOLD_FILE, PULL_HOLD_FILE] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(name), b"{}").unwrap();
            let env = [
                ("SOURCE_SERVER", "invalid-server-url"),
                ("SOURCE_CLIENT_ID", "user.source"),
                ("SOURCE_CLIENT_SECRET", "synthetic-secret"),
                ("SOURCE_PASSWORD", "synthetic-password"),
                ("DEST_SERVER", "invalid-server-url"),
                ("DEST_CLIENT_ID", "user.dest"),
                ("DEST_CLIENT_SECRET", "synthetic-secret"),
                ("DEST_PASSWORD", "synthetic-password"),
                ("BACKUP_ENABLED", "false"),
                ("BACKUPS_DIR", dir.path().to_str().unwrap()),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
            let mut config = Config::from_env(&env).unwrap();
            let status = run_once(
                &config,
                RunFlags {
                    allow_mass_delete: true,
                    reverse: true,
                    pull: true,
                    ..RunFlags::default()
                },
                &Quiet,
            )
            .await;
            assert_eq!(status.failure_stage.as_deref(), Some("recovery hold"));
            assert!(status.error.unwrap().contains(name));
            assert!(!dir.path().join("key.age").exists());
            config.dest = None;
            let status = run_once(&config, RunFlags::default(), &Quiet).await;
            assert_eq!(status.failure_stage.as_deref(), Some("source login"));
            assert!(dir.path().join(name).is_file());
        }
    }

    #[test]
    fn a_restore_hold_preserves_existing_recovery_information() {
        let dir = tempfile::tempdir().unwrap();
        hold_after_restore(dir.path()).unwrap();
        assert!(check_recovery_holds(dir.path()).is_err());
        let path = dir.path().join(RESTORE_HOLD_FILE);
        fs::write(&path, b"operator recovery notes").unwrap();
        hold_after_restore(dir.path()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"operator recovery notes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_restore_hold_symlink_still_pauses_mirroring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESTORE_HOLD_FILE);
        std::os::unix::fs::symlink(dir.path().join("missing"), &path).unwrap();
        assert!(check_recovery_holds(dir.path()).is_err());
        assert!(hold_after_restore(dir.path()).is_err());
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn a_directory_cannot_stand_in_for_a_persisted_restore_hold() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(RESTORE_HOLD_FILE)).unwrap();
        assert!(check_recovery_holds(dir.path()).is_err());
        assert!(hold_after_restore(dir.path()).is_err());
    }

    #[tokio::test]
    async fn a_notification_redirect_is_reported_as_a_delivery_failure() {
        use crate::api::tests::{http_response, http_server};
        struct Warnings(std::sync::Mutex<Vec<String>>);
        impl Reporter for Warnings {
            fn status(&self, _: &str, _: &str) {}
            fn warn(&self, warning: &str) {
                self.0.lock().unwrap().push(warning.to_string());
            }
        }
        let (url, received) = http_server(vec![http_response(
            "307 Temporary Redirect",
            "Location: https://example.invalid/secret-token\r\n",
            "",
        )]);
        let env = [
            ("SOURCE_SERVER", "invalid-server-url"),
            ("SOURCE_CLIENT_ID", "user.test"),
            ("SOURCE_CLIENT_SECRET", "synthetic-secret"),
            ("SOURCE_PASSWORD", "synthetic-password"),
            ("BACKUP_PASSPHRASE", "synthetic-passphrase"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let mut config = Config::from_env(&env).unwrap();
        config.notify_success_url = Some(url);
        let warnings = Warnings(std::sync::Mutex::new(Vec::new()));
        notify(&config, &success(), &warnings).await;
        assert_eq!(received.join().unwrap().len(), 1);
        let messages = warnings.0.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("307"));
        assert!(!messages[0].contains("secret-token"));
    }

    #[tokio::test]
    async fn a_private_key_given_as_a_recipient_is_not_repeated() {
        use age::secrecy::ExposeSecret;

        use crate::api::tests::{http_response, http_server};
        let dir = tempfile::tempdir().unwrap();
        let private = age::x25519::Identity::generate()
            .to_string()
            .expose_secret()
            .to_owned();
        let (url, received) = http_server(vec![http_response("204 No Content", "", "")]);
        let env = [
            ("SOURCE_SERVER", "http://127.0.0.1:9"),
            ("SOURCE_CLIENT_ID", "user.test"),
            ("SOURCE_CLIENT_SECRET", "synthetic-secret"),
            ("SOURCE_PASSWORD", "synthetic-password"),
            ("BACKUPS_DIR", dir.path().to_str().unwrap()),
            ("BACKUP_RECIPIENTS", &private),
            ("NOTIFY_FAILURE_URL", &url),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let config = Config::from_env(&env).unwrap();

        let status = run_once(&config, RunFlags::default(), &Quiet).await;
        assert!(!status.succeeded());
        let error = status.error.unwrap();
        // A failure message here would repeat the key, so none is printed.
        assert!(!error.contains(&private));
        assert!(error.contains("BACKUP_RECIPIENTS"));
        let written = fs::read_to_string(status_path(dir.path())).unwrap();
        assert!(!written.contains(&private));
        let requests = received.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains(&private));
    }

    #[test]
    fn descriptions_are_one_line_summaries() {
        assert_eq!(
            describe(&success()),
            "vault-mirror succeeded; 2 items created, 0 updated, 0 deleted, 40 unchanged"
        );
        let failed = Status {
            status: RunResult::Failed,
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
