//! Writes a snapshot back into a vault.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;

use crate::{
    mirror::{Endpoint, Reporter, Tally, refused},
    node::Node,
    snapshot::{Restorable, RestorableItem},
    store::Store,
    text::count,
    vault::{Draft, Scope},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct RestoreReport {
    pub items: usize,
    /// Items left out because the target server would not show them.
    pub items_left_out: usize,
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
            "the target vault already holds {}; pass --allow-non-empty to add the snapshot to them",
            count(live, "item")
        );
    }

    let mut report = RestoreReport::default();
    // Items the server would accept but not show could not be found, and
    // a second restore would create them again.
    let traits = account
        .server_traits()
        .await
        .context("reading the target server's settings")?;
    let item_type = |item: &RestorableItem| match item.content.get("type") {
        Some(Node::Number(number)) => number.as_u64(),
        _ => None,
    };
    let hidden = |item: &&RestorableItem| {
        item_type(item).is_some_and(|kind| traits.hidden_item_types.contains(&kind))
            || (snapshot.organization
                && traits.items_need_collections
                // Only collections the snapshot lists are restored.
                && !item
                    .collection_ids
                    .iter()
                    .any(|id| snapshot.collections.iter().any(|c| c.id == *id)))
    };
    let (left_out, items): (Vec<&RestorableItem>, Vec<&RestorableItem>) =
        snapshot.items.iter().partition(hidden);
    report.items_left_out = left_out.len();
    if !left_out.is_empty() {
        reporter.warn(&format!(
            "Left {} out of the restore because the target server would not show them: \
             bank accounts, driver's licences and passports are not shown yet, and organization \
             items must be in a collection.",
            count(left_out.len(), "item")
        ));
    }
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

    let drafts: Vec<Draft> = items
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
    let writes = Tally::new(reporter, "Restoring", drafts.len(), "items");
    let created = account
        .create_items(scope, &drafts, &folder_names, concurrency, &|n| {
            writes.add(n)
        })
        .await?;

    // Restored and missing attachments of each item.
    let outcomes: Vec<(usize, usize)> = stream::iter(items.iter().zip(&created).map(
        |(item, created)| async move {
            let (mut restored, mut missing) = (0, 0);
            if !attachments_allowed {
                return anyhow::Ok((0, 0));
            }
            for attachment in &item.attachments {
                let data = match store {
                    Some(store)
                        if attachment.stored && store.has_attachment(&attachment.attachment_id) =>
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
            anyhow::Ok((restored, missing))
        },
    ))
    .buffer_unordered(concurrency)
    .try_collect()
    .await?;

    let archived: Vec<String> = items
        .iter()
        .zip(&created)
        .filter(|(item, _)| item.archived)
        .map(|(_, created)| created.id.clone())
        .collect();
    if !archived.is_empty()
        && let Err(error) = account.set_archived(&archived, true).await
    {
        // As when mirroring, only a refusal is tolerated.
        if !refused(&error) {
            return Err(error.context("archiving restored items"));
        }
        reporter.warn(&format!(
            "The server refused to archive {}, so they stay unarchived: {error:#}",
            count(archived.len(), "restored item")
        ));
    }
    let visible: HashSet<String> = account
        .load(scope)
        .await
        .context("reading the target vault again")?
        .items
        .into_iter()
        .map(|item| item.id)
        .collect();
    let invisible = created
        .iter()
        .filter(|item| !visible.contains(&item.id))
        .count();
    if invisible > 0 {
        bail!(
            "the server accepted {} but does not show them in the target vault",
            count(invisible, "restored item")
        );
    }
    report.items = created.len();
    report.attachments_restored = outcomes.iter().map(|(restored, _)| restored).sum();
    report.attachments_missing = outcomes.iter().map(|(_, missing)| missing).sum();
    if !attachments_allowed {
        report.attachments_skipped = items.iter().map(|item| item.attachments.len()).sum();
    }
    if report.attachments_skipped > 0 {
        reporter.warn(&format!(
            "Skipped {}: the account has no premium subscription, which Bitwarden requires \
             for attachments.",
            count(report.attachments_skipped, "attachment")
        ));
    }
    if report.attachments_missing > 0 {
        reporter.warn(&format!(
            "The backup lacks the content of {} listed in the snapshot.",
            count(report.attachments_missing, "attachment")
        ));
    }
    Ok(report)
}
