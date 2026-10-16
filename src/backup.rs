//! Writes an encrypted snapshot of one vault.

use anyhow::{Context, Result};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{
    mirror::{Reporter, Tally},
    snapshot::{self, AttachmentStorage, SnapshotAttachment},
    store::{PruneReport, Retention, SnapshotWriter, Store, VaultOrigin},
    text::count,
    vault::{Account, Attachment, Item, Scope, Vault},
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
    let origin = VaultOrigin {
        api_url: account.api_url().to_string(),
        account_id: account.user_id.clone(),
        organization_id: scope.organization_id().map(str::to_string),
    };
    let live: Vec<_> = vault.items.iter().filter(|item| !item.in_trash).collect();

    let mut records = Vec::new();
    let mut files = Vec::new();
    for item in &live {
        for attachment in &item.attachments {
            if options.attachments {
                files.push((records.len(), *item, attachment));
            }
            records.push(SnapshotAttachment {
                item_id: item.id.clone(),
                attachment_id: attachment.id.clone(),
                file_name: attachment.file_name.clone(),
                size: attachment.size,
                storage: AttachmentStorage::Omitted,
            });
        }
    }
    // An attachment's content never changes under one id, so the records
    // tell whether the newest snapshot holds this vault before any download.
    let planned = snapshot::build(vault, scope, &records)?;
    let (snapshot, downloaded) = match store.unchanged(&origin, &planned, options.attachments)? {
        Some(_) => {
            reporter.status(
                "Skipped",
                "a new snapshot; the vault is unchanged since the newest one",
            );
            (None, 0)
        }
        None => {
            let writer = store.begin_snapshot(&origin, options.now)?;
            let downloaded = files.len();
            for (index, storage) in download(account, scope, &writer, files, reporter).await? {
                records[index].storage = storage;
            }
            if downloaded > 0 {
                reporter.status("Backed up", &count(downloaded, "attachment"));
            }
            let path = writer.finish(snapshot::build(vault, scope, &records)?)?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            reporter.status(
                "Saved",
                &format!("snapshot {name} with {}", count(live.len(), "item")),
            );
            (Some(name), downloaded)
        }
    };

    let pruned = match options.retention {
        Some(retention) => store.prune(&origin, retention, options.now)?,
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

/// Downloads each attachment into `writer`, and returns where the content of
/// each record, by index, now is.
async fn download(
    account: &Account,
    scope: &Scope,
    writer: &SnapshotWriter<'_>,
    files: Vec<(usize, &Item, &Attachment)>,
    reporter: &dyn Reporter,
) -> Result<Vec<(usize, AttachmentStorage)>> {
    let downloads = Tally::new(reporter, "Downloading", files.len(), "attachments");
    let downloads = &downloads;
    stream::iter(
        files
            .into_iter()
            .map(|(index, item, attachment)| async move {
                let data = account
                    .download_attachment(scope, item, attachment)
                    .await
                    .with_context(|| {
                        format!(
                            "downloading attachment {} of item {}",
                            attachment.id, item.id
                        )
                    })?;
                let storage = writer.write_attachment(&attachment.id, &data)?;
                downloads.add(1);
                anyhow::Ok((index, storage))
            }),
    )
    .buffer_unordered(2)
    .try_collect()
    .await
}
