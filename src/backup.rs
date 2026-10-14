//! Writes an encrypted snapshot of one vault.

use anyhow::{Context, Result};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{
    mirror::Reporter,
    snapshot::{self, SnapshotAttachment},
    store::{PruneReport, Retention, Store},
    vault::{Account, Scope},
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
        store.write_attachment(&attachment.id, &data)
    }))
    .buffer_unordered(2)
    .try_collect::<()>()
    .await?;
    if downloaded > 0 {
        reporter.info(&format!("Backed up {downloaded} new attachment(s)."));
    }

    let stored_ids = records
        .iter()
        .filter(|r| r.stored)
        .map(|r| r.attachment_id.clone())
        .collect();
    let document = snapshot::build(&vault, scope, &records);
    let written = store.write_snapshot(document, stored_ids, options.now, snapshot::stamp)?;
    let snapshot = written.and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    match &snapshot {
        Some(name) => reporter.info(&format!(
            "Wrote snapshot {name} with {} item(s).",
            live.len()
        )),
        None => reporter
            .info("The vault is unchanged since the newest snapshot; no new snapshot written."),
    }

    let pruned = match options.retention {
        Some(retention) => store.prune(retention, options.now)?,
        None => PruneReport::default(),
    };
    if pruned != PruneReport::default() {
        reporter.info(&format!(
            "Pruned {} snapshot(s) and {} attachment file(s).",
            pruned.snapshots_removed, pruned.attachments_removed
        ));
    }
    Ok(BackupReport {
        snapshot,
        items: live.len(),
        attachments_downloaded: downloaded,
        pruned,
    })
}
