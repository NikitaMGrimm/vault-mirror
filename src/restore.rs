//! Writes a snapshot back into a vault.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;

use crate::{
    mirror::{Endpoint, Reporter},
    snapshot::Restorable,
    store::Store,
    vault::{Draft, Scope},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct RestoreReport {
    pub items: usize,
    pub folders_created: usize,
    pub collections_created: usize,
    pub attachments_restored: usize,
    /// Attachments the snapshot lists but whose content is not in the backup.
    pub attachments_missing: usize,
    /// Attachments left out because the account may not upload attachments.
    pub attachments_skipped: usize,
}

pub struct RestoreOptions {
    /// Restore into a vault that already has items. They are kept, so
    /// restoring the same snapshot twice duplicates everything.
    pub allow_non_empty: bool,
    pub concurrency: usize,
}

pub async fn restore(
    target: &Endpoint<'_>,
    snapshot: &Restorable,
    store: Option<&Store<'_>>,
    options: &RestoreOptions,
    reporter: &dyn Reporter,
) -> Result<RestoreReport> {
    let account = target.account;
    let scope = &target.scope;
    match (snapshot.organization, scope) {
        (false, Scope::Personal) | (true, Scope::Organization(_)) => {}
        (true, Scope::Personal) => {
            bail!("this snapshot is of an organization; restore it into an organization")
        }
        (false, Scope::Organization(_)) => {
            bail!("this snapshot is of an individual vault; restore it into an individual vault")
        }
    }
    let existing = account
        .load(scope)
        .await
        .context("reading the target vault")?;
    let live = existing.items.iter().filter(|item| !item.in_trash).count();
    if live > 0 && !options.allow_non_empty {
        bail!(
            "the target vault already holds {live} item(s); pass --allow-non-empty to add the snapshot to them"
        );
    }

    let mut report = RestoreReport::default();
    // Snapshot container id -> target container id, matched by name.
    let mut folder_ids: HashMap<&str, String> = HashMap::new();
    let mut collection_ids: HashMap<&str, String> = HashMap::new();
    match scope {
        Scope::Personal => {
            let mut by_name: HashMap<String, String> = existing
                .folders
                .into_iter()
                .map(|f| (f.name, f.id))
                .collect();
            for folder in &snapshot.folders {
                let id = match by_name.get(&folder.name) {
                    Some(id) => id.clone(),
                    None => {
                        report.folders_created += 1;
                        let created = account.create_folder(&folder.name).await?;
                        by_name.insert(created.name, created.id.clone());
                        created.id
                    }
                };
                folder_ids.insert(&folder.id, id);
            }
        }
        Scope::Organization(org) => {
            let mut by_name: HashMap<String, String> = existing
                .collections
                .into_iter()
                .map(|c| (c.name, c.id))
                .collect();
            for collection in &snapshot.collections {
                let id = match by_name.get(&collection.name) {
                    Some(id) => id.clone(),
                    None => {
                        report.collections_created += 1;
                        let created = account
                            .create_collection(
                                org,
                                &collection.name,
                                collection.external_id.as_deref(),
                            )
                            .await?;
                        by_name.insert(created.name, created.id.clone());
                        created.id
                    }
                };
                collection_ids.insert(&collection.id, id);
            }
        }
    }

    let drafts: Vec<Draft> = snapshot
        .items
        .iter()
        .map(|item| Draft {
            content: item.content.clone(),
            folder_id: item
                .folder_id
                .as_deref()
                .and_then(|id| folder_ids.get(id))
                .cloned(),
            collection_ids: item
                .collection_ids
                .iter()
                .filter_map(|id| collection_ids.get(id.as_str()))
                .cloned()
                .collect(),
        })
        .collect();
    let folder_names: HashMap<String, String> = snapshot
        .folders
        .iter()
        .filter_map(|folder| {
            Some((
                folder_ids.get(folder.id.as_str())?.clone(),
                folder.name.clone(),
            ))
        })
        .collect();
    let concurrency = options.concurrency.max(1);
    // Bitwarden accepts attachments only from premium accounts.
    let attachments_allowed = *scope != Scope::Personal || account.premium;
    let created = account
        .create_items(scope, &drafts, &folder_names, concurrency)
        .await?;

    let outcomes: Vec<(bool, String, usize, usize)> = stream::iter(
        snapshot
            .items
            .iter()
            .zip(&created)
            .map(|(item, created)| async move {
                let (mut restored, mut missing) = (0, 0);
                if !attachments_allowed {
                    return anyhow::Ok((item.archived, created.id.clone(), 0, 0));
                }
                for attachment in &item.attachments {
                    let data = match store {
                        Some(store)
                            if attachment.stored
                                && store.has_attachment(&attachment.attachment_id) =>
                        {
                            store.read_attachment(&attachment.attachment_id)?
                        }
                        _ => {
                            missing += 1;
                            continue;
                        }
                    };
                    account
                        .upload_attachment(scope, created, &attachment.file_name, &data)
                        .await
                        .with_context(|| {
                            format!("restoring an attachment of snapshot item {}", item.id)
                        })?;
                    restored += 1;
                }
                anyhow::Ok((item.archived, created.id.clone(), restored, missing))
            }),
    )
    .buffer_unordered(concurrency)
    .try_collect()
    .await?;

    let archived: Vec<String> = outcomes
        .iter()
        .filter(|(archived, ..)| *archived)
        .map(|(_, id, ..)| id.clone())
        .collect();
    if !archived.is_empty()
        && let Err(error) = account.set_archived(&archived, true).await
    {
        reporter.warn(&format!(
                "The server refused to archive {} restored item(s); they were restored unarchived: {error:#}",
                archived.len()
            ));
    }
    report.items = outcomes.len();
    report.attachments_restored = outcomes.iter().map(|(_, _, restored, _)| restored).sum();
    report.attachments_missing = outcomes.iter().map(|(_, _, _, missing)| missing).sum();
    if !attachments_allowed {
        report.attachments_skipped = snapshot
            .items
            .iter()
            .map(|item| item.attachments.len())
            .sum();
    }
    if report.attachments_skipped > 0 {
        reporter.warn(&format!(
            "{} attachment(s) are not restored because the account has no premium subscription, \
             which Bitwarden requires for attachments.",
            report.attachments_skipped
        ));
    }
    if report.attachments_missing > 0 {
        reporter.warn(&format!(
            "{} attachment(s) are listed in the snapshot but their content is not in the backup.",
            report.attachments_missing
        ));
    }
    Ok(report)
}
