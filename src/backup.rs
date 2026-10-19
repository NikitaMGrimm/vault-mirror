//! Writes an encrypted snapshot of one vault.

use anyhow::{Context, Result};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    mirror::{Protected, Reporter, Tally},
    snapshot,
    store::Store,
    text::count,
    vault::{Account, Scope},
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

/// Reads the vault and backs up exactly what it read. Returns that read
/// with the snapshot that holds it, a new one or, for an unchanged vault,
/// the newest.
pub async fn backup<'k>(
    account: &Account,
    scope: &Scope,
    store: &Store<'k>,
    options: &BackupOptions,
    reporter: &dyn Reporter,
) -> Result<(BackupReport, Protected<'k>)> {
    let read = account
        .load_with_traits(scope)
        .await
        .context("reading the vault")?;
    let vault = read.vault();
    let live: Vec<_> = vault.items.iter().filter(|item| !item.in_trash).collect();
    let document = snapshot::build(vault, scope)?;
    let mut report = BackupReport {
        items: live.len(),
        ..BackupReport::default()
    };
    // An attachment's content never changes under one id, so the document
    // tells whether the newest snapshot holds this vault before any download.
    let id = if store.unchanged(&document)? {
        reporter.status(
            "Skipped",
            "a new snapshot; the vault is unchanged since the newest one",
        );
        store.list()?.pop().context("the newest snapshot is gone")?
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
        report.snapshot = Some(id.clone());
        report.attachments_downloaded = files.len();
        id
    };

    // The newest snapshot, the one just used, always stays.
    report.snapshots_pruned = store.prune(options.retention_days, options.now)?;
    if report.snapshots_pruned > 0 {
        reporter.status("Pruned", &count(report.snapshots_pruned, "snapshot"));
    }
    let snapshot = store.snapshot(&id);
    Ok((report, Protected { read, snapshot }))
}
