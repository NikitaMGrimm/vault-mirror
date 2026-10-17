//! Writes an encrypted snapshot of one vault.

use anyhow::{Context, Result};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    mirror::{Reporter, Tally},
    snapshot,
    store::Store,
    text::count,
    vault::{Account, Scope, Vault},
};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BackupReport {
    /// Id of the snapshot written, or `None` when the vault had not changed
    /// since the newest snapshot.
    pub snapshot: Option<String>,
    pub items: usize,
    pub attachments_downloaded: usize,
    pub snapshots_pruned: usize,
}

pub struct BackupOptions {
    pub retention_days: u32,
    pub now: OffsetDateTime,
}

pub async fn backup(
    account: &Account,
    scope: &Scope,
    store: &Store<'_>,
    options: &BackupOptions,
    reporter: &dyn Reporter,
) -> Result<BackupReport> {
    let vault = account.load(scope).await.context("reading the vault")?;
    backup_loaded(account, scope, &vault, store, options, reporter).await
}

/// Backs up `vault`, as just read from `account`.
pub async fn backup_loaded(
    account: &Account,
    scope: &Scope,
    vault: &Vault,
    store: &Store<'_>,
    options: &BackupOptions,
    reporter: &dyn Reporter,
) -> Result<BackupReport> {
    let live: Vec<_> = vault.items.iter().filter(|item| !item.in_trash).collect();
    let document = snapshot::build(vault, scope)?;
    let mut report = BackupReport {
        items: live.len(),
        ..BackupReport::default()
    };
    // An attachment's content never changes under one id, so the document
    // tells whether the newest snapshot holds this vault before any download.
    if store.unchanged(&document)? {
        reporter.status(
            "Skipped",
            "a new snapshot; the vault is unchanged since the newest one",
        );
    } else {
        let writer = store.begin(options.now)?;
        let files: Vec<_> = (live.iter())
            .flat_map(|item| item.attachments.iter().map(move |a| (*item, a)))
            .collect();
        let downloads = Tally::new(reporter, "Downloading", files.len(), "attachments");
        let (to, downloads) = (&writer, &downloads);
        stream::iter(files.iter().map(|(item, attachment)| async move {
            let data = account
                .download_attachment(scope, item, attachment)
                .await
                .with_context(|| {
                    format!(
                        "downloading attachment {} of item {}",
                        attachment.id, item.id
                    )
                })?;
            to.write_attachment(&attachment.id, &data)?;
            downloads.add(1);
            anyhow::Ok(())
        }))
        .buffer_unordered(2)
        .try_collect::<Vec<()>>()
        .await?;
        if !files.is_empty() {
            reporter.status("Backed up", &count(files.len(), "attachment"));
        }
        let id = writer.finish(&document)?;
        reporter.status(
            "Saved",
            &format!("snapshot {id} with {}", count(live.len(), "item")),
        );
        report.snapshot = Some(id);
        report.attachments_downloaded = files.len();
    }

    report.snapshots_pruned = store.prune(options.retention_days, options.now)?;
    if report.snapshots_pruned > 0 {
        reporter.status("Pruned", &count(report.snapshots_pruned, "snapshot"));
    }
    Ok(report)
}
