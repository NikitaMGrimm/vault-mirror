//! Writes a snapshot back into a vault.
//!
//! Every attachment file is read from the backup before the first write, so a
//! damaged backup restores nothing. Afterwards the target vault is read again
//! and every restored item compared with the snapshot: a server that accepts
//! an item but keeps less of it fails the restore. Nothing written is undone,
//! so a failed restore leaves its items in the target vault to inspect.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};

use crate::{
    mirror::{Endpoint, Reporter, Tally, refused},
    node::Node,
    plan::{Mode, comparable, differing_paths},
    snapshot::{self, RestorableItem, SnapshotAttachment},
    store::Snapshot,
    text::count,
    vault::{Draft, Item, Scope, Vault, WRITES_AT_ONCE},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestoreReport {
    pub items: usize,
    /// Items left out because the target server would not show them.
    pub items_left_out: usize,
    /// Restored items that stay unarchived because the server refused to
    /// archive them.
    pub items_left_unarchived: usize,
    pub folders_created: usize,
    pub collections_created: usize,
    pub attachments_restored: usize,
    /// Attachments left out because the account may not upload attachments.
    pub attachments_skipped: usize,
}

pub struct RestoreOptions {
    /// Restore into a vault that already has items. They are kept, so
    /// restoring the same snapshot twice duplicates everything.
    pub allow_non_empty: bool,
}

pub async fn restore(
    target: &Endpoint<'_>,
    backup: &Snapshot<'_>,
    options: &RestoreOptions,
    reporter: &dyn Reporter,
) -> Result<RestoreReport> {
    let snapshot = &snapshot::parse(&backup.document()?)?;
    let account = target.account;
    let scope = &target.scope;
    match (snapshot.organization(), scope) {
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
    let read = |attachment: &SnapshotAttachment| {
        backup
            .attachment(&attachment.attachment_id)
            .with_context(|| {
                format!(
                    "reading attachment {} of snapshot item {} from the backup",
                    attachment.attachment_id, attachment.item_id
                )
            })
    };
    for attachment in snapshot.attachments() {
        read(attachment)?;
    }

    // Items the server would accept but not show could not be found, and
    // a second restore would create them again.
    let traits = account
        .server_traits()
        .await
        .context("reading the target server's settings")?;
    let hidden = |item: &RestorableItem| {
        let kind = match item.content.get("type") {
            Some(Node::Number(number)) => number.as_u64(),
            _ => None,
        };
        kind.is_some_and(|kind| traits.hidden_item_types.contains(&kind))
            || (snapshot.organization()
                && traits.items_need_collections
                && item.collection_ids.is_empty())
    };
    // Bitwarden accepts attachments only from premium accounts.
    let attachments_allowed = *scope != Scope::Personal || account.premium;
    let mut report = RestoreReport::default();
    let mut items = Vec::new();
    for item in snapshot.items() {
        if hidden(item) {
            report.items_left_out += 1;
            continue;
        }
        items.push(item);
        if !attachments_allowed {
            report.attachments_skipped += item.attachments.len();
        }
    }
    if report.items_left_out > 0 {
        reporter.warn(&format!(
            "Left {} out of the restore because the target server would not show them: \
             bank accounts, driver's licences and passports are not shown yet, and organization \
             items must be in a collection.",
            count(report.items_left_out, "item")
        ));
    }
    if report.attachments_skipped > 0 {
        reporter.warn(&format!(
            "Skipped {}: the account has no premium subscription, which Bitwarden requires \
             for attachments.",
            count(report.attachments_skipped, "attachment")
        ));
    }

    // Snapshot container id -> target container id, matched by name.
    let mut folder_ids: HashMap<&str, String> = HashMap::new();
    let mut collection_ids: HashMap<&str, String> = HashMap::new();
    match scope {
        Scope::Personal => {
            let mut by_name: HashMap<String, String> = (existing.folders.iter())
                .map(|f| (f.name.clone(), f.id.clone()))
                .collect();
            for folder in snapshot.folders() {
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
            let mut by_name: HashMap<String, String> = (existing.collections.iter())
                .map(|c| (c.name.clone(), c.id.clone()))
                .collect();
            for collection in snapshot.collections() {
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
        .map(|item| {
            let mut content = item.content.clone();
            // Servers materialize these defaults. Sending them keeps
            // verification strict for every explicitly saved value.
            content
                .entry("reprompt".into())
                .or_insert(Node::Number(0.into()));
            if *scope == Scope::Personal {
                content
                    .entry("favorite".into())
                    .or_insert(Node::Bool(false));
            }
            Draft {
                content,
                folder_id: (item.folder_id.as_deref())
                    .and_then(|id| folder_ids.get(id))
                    .cloned(),
                collection_ids: (item.collection_ids.iter())
                    .filter_map(|id| collection_ids.get(id.as_str()))
                    .cloned()
                    .collect(),
            }
        })
        .collect();
    let folder_names: HashMap<String, String> = (snapshot.folders().iter())
        .filter_map(|folder| {
            Some((
                folder_ids.get(folder.id.as_str())?.clone(),
                folder.name.clone(),
            ))
        })
        .collect();
    let writes = Tally::new(reporter, "Restoring", drafts.len(), "items");
    let created = account
        .create_items(scope, &drafts, &folder_names, &|n| writes.add(n))
        .await?;

    let files: Vec<Vec<String>> = stream::iter(items.iter().zip(&created).map(
        |(item, created)| async move {
            let mut names = Vec::new();
            for attachment in (item.attachments.iter()).filter(|_| attachments_allowed) {
                let data = read(attachment)?;
                account
                    .upload_attachment(scope, created, &attachment.file_name, &data)
                    .await
                    .with_context(|| {
                        format!(
                            "restoring attachment {} of snapshot item {}",
                            attachment.attachment_id, item.id
                        )
                    })?;
                names.push(attachment.file_name.clone());
            }
            anyhow::Ok(names)
        },
    ))
    .buffered(WRITES_AT_ONCE)
    .try_collect()
    .await?;

    let archived: Vec<String> = (items.iter().zip(&created))
        .filter(|(item, _)| item.archived)
        .map(|(_, created)| created.id.clone())
        .collect();
    let mut archive_refused = false;
    if !archived.is_empty()
        && let Err(error) = account.set_archived(&archived, true).await
    {
        // As when mirroring, only a refusal is tolerated.
        if !refused(&error) {
            return Err(error.context("archiving restored items"));
        }
        archive_refused = true;
        report.items_left_unarchived = archived.len();
        reporter.warn(&format!(
            "The server refused to archive {}, so they stay unarchived: {error:#}",
            count(archived.len(), "restored item")
        ));
    }

    let expected: Vec<(Item, Vec<String>)> = (items.iter().zip(drafts).zip(created).zip(files))
        .map(|(((item, draft), created), files)| {
            let item = Item {
                folder_id: draft.folder_id,
                collection_ids: draft.collection_ids,
                archived: item.archived && !archive_refused,
                content: draft.content,
                ..created
            };
            (item, files)
        })
        .collect();
    let vault = account
        .load(scope)
        .await
        .context("reading the target vault again")?;
    let mode = match scope {
        Scope::Personal => Mode::Personal,
        Scope::Organization(_) => Mode::Organization,
    };
    let mut problems = differences(&expected, &vault, mode);
    if !problems.is_empty() {
        let more = match problems.len() {
            0..=5 => String::new(),
            n => format!("; and {} more", n - 5),
        };
        problems.truncate(5);
        bail!(
            "the target vault does not hold what the restore wrote: {}{more}. The {} stay \
             there for you to check",
            problems.join("; "),
            count(expected.len(), "restored item")
        );
    }
    report.items = expected.len();
    report.attachments_restored = expected.iter().map(|(_, files)| files.len()).sum();
    Ok(report)
}

/// What differs between the restored items as written, with the names of
/// their uploaded files, and `vault`, as paths and ids only: this ends up in
/// logs.
fn differences(expected: &[(Item, Vec<String>)], vault: &Vault, mode: Mode) -> Vec<String> {
    let by_id: HashMap<&str, &Item> = (vault.items.iter())
        .filter(|item| !item.in_trash)
        .map(|item| (item.id.as_str(), item))
        .collect();
    let shape = |item: &Item| {
        let mut collections = item.collection_ids.clone();
        collections.sort();
        comparable(item, item.folder_id.as_deref(), &collections, mode)
    };
    let mut problems = Vec::new();
    for (wanted, files) in expected {
        let id = &wanted.id;
        let Some(found) = by_id.get(id.as_str()) else {
            problems.push(format!("item {id}: the target vault does not show it"));
            continue;
        };
        let mut reasons = Vec::new();
        let paths = differing_paths(&shape(wanted), &shape(found));
        if !paths.is_empty() {
            reasons.push(format!("it differs at [{}]", paths.join(", ")));
        }
        if found.archived != wanted.archived {
            reasons.push("its archive state differs".to_string());
        }
        let mut uploaded: Vec<&str> = files.iter().map(String::as_str).collect();
        let mut stored: Vec<&str> = (found.attachments.iter())
            .map(|a| a.file_name.as_str())
            .collect();
        uploaded.sort_unstable();
        stored.sort_unstable();
        if uploaded != stored {
            reasons.push("its attachments differ".to_string());
        }
        if !reasons.is_empty() {
            problems.push(format!("item {id}: {}", reasons.join(", ")));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::vault::Attachment;

    fn note(id: &str) -> Item {
        Item {
            id: id.to_string(),
            folder_id: Some("folder".into()),
            collection_ids: Vec::new(),
            revision_date: None,
            creation_date: None,
            in_trash: false,
            archived: false,
            archived_date: None,
            content: BTreeMap::from([
                ("type".to_string(), Node::Number(2.into())),
                ("name".to_string(), Node::secret("Note")),
                ("notes".to_string(), Node::secret("written down")),
                ("favorite".to_string(), Node::Bool(false)),
                (
                    "secureNote".to_string(),
                    Node::Map(BTreeMap::from([(
                        "type".to_string(),
                        Node::Number(0.into()),
                    )])),
                ),
            ]),
            attachments: Vec::new(),
            wrapped_key: None,
        }
    }

    fn files() -> Vec<String> {
        vec!["scan.pdf".to_string(), "photo.jpg".to_string()]
    }

    /// The restored item as a server that kept everything shows it.
    fn kept() -> Item {
        let mut item = note("i");
        item.attachments = (files().into_iter().enumerate())
            .map(|(n, file_name)| Attachment {
                id: format!("a{n}"),
                file_name,
                size: 100,
                wrapped_key: None,
            })
            .collect();
        item
    }

    fn check(expected: Item, found: Item, mode: Mode) -> Vec<String> {
        let vault = Vault {
            items: vec![found],
            ..Vault::default()
        };
        differences(&[(expected, files())], &vault, mode)
    }

    #[test]
    fn an_item_that_kept_everything_passes_whatever_the_server_adds() {
        let mut found = kept();
        // Servers return empty lists for what was left out, and their own dates.
        found
            .content
            .insert("fields".into(), Node::List(Vec::new()));
        found.revision_date = Some("2026-10-16T12:00:00.000Z".into());
        assert_eq!(
            check(note("i"), found, Mode::Personal),
            Vec::<String>::new()
        );
    }

    #[test]
    fn whatever_the_server_dropped_or_changed_is_named_by_path_and_id() {
        type Case = (&'static str, fn(&mut Item), &'static str);
        let cases: Vec<Case> = vec![
            (
                "dropped notes",
                |item| _ = item.content.remove("notes"),
                "differs at [content.notes]",
            ),
            (
                "other folder",
                |item| item.folder_id = None,
                "differs at [folder]",
            ),
            (
                "archived",
                |item| item.archived = true,
                "archive state differs",
            ),
            (
                "dropped file",
                |item| _ = item.attachments.pop(),
                "attachments differ",
            ),
            (
                "renamed file",
                |item| item.attachments[1].file_name = "other.pdf".into(),
                "attachments differ",
            ),
            (
                "extra file",
                |item| item.attachments.push(item.attachments[0].clone()),
                "attachments differ",
            ),
            (
                "in the trash",
                |item| item.in_trash = true,
                "does not show it",
            ),
            (
                "another item",
                |item| item.id = "j".into(),
                "does not show it",
            ),
        ];
        for (what, change, reason) in cases {
            let mut found = kept();
            change(&mut found);
            let problems = check(note("i"), found, Mode::Personal);
            assert_eq!(problems.len(), 1, "{what}");
            assert!(problems[0].starts_with("item i: "), "{what}");
            assert!(problems[0].contains(reason), "{what}: {}", problems[0]);
            assert!(!problems[0].contains("written down"), "{what}");
        }
    }

    #[test]
    fn organization_items_are_checked_by_collection_without_favorites() {
        let mut expected = note("i");
        expected.collection_ids = vec!["c2".into(), "c1".into()];
        let mut found = kept();
        found.collection_ids = vec!["c1".into()];
        let problems = check(expected.clone(), found.clone(), Mode::Organization);
        assert!(
            problems[0].contains("differs at [collections]"),
            "{problems:?}"
        );

        found.collection_ids = vec!["c1".into(), "c2".into()];
        // Bitwarden keeps an organization item's favorite flag per member.
        found.content.insert("favorite".into(), Node::Bool(true));
        assert_eq!(
            check(expected, found, Mode::Organization),
            Vec::<String>::new()
        );
    }
}
