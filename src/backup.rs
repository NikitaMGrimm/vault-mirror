//! Writes an encrypted snapshot of one vault.

use anyhow::{Context, Result};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{
    mirror::{Reporter, Tally},
    snapshot::{self, SnapshotAttachment},
    store::{PruneReport, Retention, Store},
    text::count,
    vault::{Account, Scope, Vault},
};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct BackupReport {
    /// File name of the snapshot written, or `None` when the vault had not
    /// changed since the newest snapshot.
    pub snapshot: Option<String>,
    pub items: usize,
    pub attachments_downloaded: usize,
    pub pruned: PruneReport,
}

pub struct BackupOptions<'a> {
    /// Store attachment content, not only a record of each attachment.
    pub attachments: bool,
    pub retention: Option<&'a Retention>,
    pub now: OffsetDateTime,
}

pub async fn backup(
    account: &Account,
    scope: &Scope,
    store: &Store<'_>,
    options: &BackupOptions<'_>,
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
    options: &BackupOptions<'_>,
    reporter: &dyn Reporter,
) -> Result<BackupReport> {
    let live: Vec<_> = vault.items.iter().filter(|item| !item.in_trash).collect();

    let mut records = Vec::new();
    let mut missing = Vec::new();
    for item in &live {
        for attachment in &item.attachments {
            records.push(SnapshotAttachment {
                item_id: item.id.clone(),
                attachment_id: attachment.id.clone(),
                file_name: attachment.file_name.clone(),
                size: attachment.size,
                stored: options.attachments,
            });
            // An attachment's content never changes under one id, so a file
            // that is already stored is never fetched again.
            if options.attachments && !store.has_attachment(&attachment.id) {
                missing.push((*item, attachment));
            }
        }
    }
    let downloaded = missing.len();
    let downloads = Tally::new(reporter, "Downloading", downloaded, "attachments");
    let downloads = &downloads;
    stream::iter(missing.into_iter().map(|(item, attachment)| async move {
        let data = account
            .download_attachment(scope, item, attachment)
            .await
            .with_context(|| {
                format!(
                    "downloading attachment {} of item {}",
                    attachment.id, item.id
                )
            })?;
        store.write_attachment(&attachment.id, &data)?;
        downloads.add(1);
        anyhow::Ok(())
    }))
    .buffer_unordered(2)
    .try_collect::<()>()
    .await?;
    if downloaded > 0 {
        reporter.status("Backed up", &count(downloaded, "new attachment"));
    }

    let stored_ids = records
        .iter()
        .filter(|r| r.stored)
        .map(|r| r.attachment_id.clone())
        .collect();
    let document = snapshot::build(vault, scope, &records);
    let written = store.write_snapshot(
        scope.organization_id().unwrap_or(&account.user_id),
        document,
        stored_ids,
        options.now,
        snapshot::stamp,
    )?;
    let snapshot = written.and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    match &snapshot {
        Some(name) => reporter.status(
            "Saved",
            &format!("snapshot {name} with {}", count(live.len(), "item")),
        ),
        None => reporter.status(
            "Skipped",
            "a new snapshot; the vault is unchanged since the newest one",
        ),
    }

    let pruned = match options.retention {
        Some(retention) => store.prune(retention, options.now)?,
        None => PruneReport::default(),
    };
    if pruned != PruneReport::default() {
        reporter.status(
            "Pruned",
            &format!(
                "{} and {}",
                count(pruned.snapshots_removed, "snapshot"),
                count(pruned.attachments_removed, "attachment file")
            ),
        );
    }
    Ok(BackupReport {
        snapshot,
        items: live.len(),
        attachments_downloaded: downloaded,
        pruned,
    })
}
