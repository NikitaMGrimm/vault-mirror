//! Writes a snapshot back into a vault.
//!
//! [`prepare`] reads everything a restore needs, decides what the target can
//! take and checks every attachment file the backup promises, without writing
//! anything. A damaged backup stops the restore before its first write. Then
//! [`PreparedRestore::apply`] writes, reads the target vault again, and checks
//! every restored item against the snapshot: a server that accepts an item but
//! keeps less of it fails the restore. Nothing written is ever undone, so a
//! failed restore leaves its items in the target vault to inspect.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    mirror::{Endpoint, Reporter, Tally, refused},
    node::Node,
    plan::{Mode, comparable, differing_paths},
    snapshot::{AttachmentStorage, Restorable, RestorableItem, SnapshotAttachment},
    store::{StagedAttachment, Store},
    text::count,
    vault::{Collection, Draft, Folder, Item, Scope, Vault},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
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
    /// Attachments the snapshot lists but whose content the backup left out.
    pub attachments_missing: usize,
    /// Attachments left out because the account may not upload attachments.
    pub attachments_skipped: usize,
}

impl RestoreReport {
    /// Whether the restore left anything out, with a warning about it.
    pub fn warned(&self) -> bool {
        self.items_left_out
            + self.items_left_unarchived
            + self.attachments_missing
            + self.attachments_skipped
            > 0
    }
}

pub struct RestoreOptions {
    /// Restore into a vault that already has items. They are kept, so
    /// restoring the same snapshot twice duplicates everything.
    pub allow_non_empty: bool,
    pub concurrency: usize,
}

/// An attachment file read from the backup and checked, ready to upload.
struct StagedFile<'a> {
    attachment: &'a SnapshotAttachment,
    staged: StagedAttachment<'a>,
    length: usize,
    sha256: [u8; 32],
}

/// A restore that has read and checked everything it needs, and written
/// nothing yet.
pub struct PreparedRestore<'a> {
    target: &'a Endpoint<'a>,
    snapshot: &'a Restorable,
    existing: Vault,
    items: Vec<(&'a RestorableItem, Vec<StagedFile<'a>>)>,
    report: RestoreReport,
    warnings: Vec<String>,
    concurrency: usize,
}

/// Reads `target` and the backup, and decides what to restore. Writes nothing.
pub async fn prepare<'a>(
    target: &'a Endpoint<'a>,
    snapshot: &'a Restorable,
    store: Option<&'a Store<'a>>,
    options: &RestoreOptions,
) -> Result<PreparedRestore<'a>> {
    check_input(snapshot)?;
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

    // Every file the backup promises is read and checked first, whatever
    // the target would take: a damaged backup is not restored at all.
    let staged = (snapshot.items.iter())
        .map(|item| {
            (item.attachments.iter())
                .filter(|attachment| attachment.storage != AttachmentStorage::Omitted)
                .map(|attachment| {
                    stage(store, attachment).with_context(|| {
                        format!(
                            "reading attachment {} of snapshot item {} from the backup",
                            attachment.attachment_id, item.id
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;

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
            || (snapshot.organization
                && traits.items_need_collections
                && item.collection_ids.is_empty())
    };
    // Bitwarden accepts attachments only from premium accounts.
    let attachments_allowed = *scope != Scope::Personal || account.premium;
    let mut report = RestoreReport::default();
    let mut prepared = Vec::new();
    for (item, files) in snapshot.items.iter().zip(staged) {
        if hidden(item) {
            report.items_left_out += 1;
        } else if attachments_allowed {
            report.attachments_missing += (item.attachments.iter())
                .filter(|attachment| attachment.storage == AttachmentStorage::Omitted)
                .count();
            prepared.push((item, files));
        } else {
            report.attachments_skipped += item.attachments.len();
            prepared.push((item, Vec::new()));
        }
    }
    let mut warnings = Vec::new();
    if report.items_left_out > 0 {
        warnings.push(format!(
            "Left {} out of the restore because the target server would not show them: \
             bank accounts, driver's licences and passports are not shown yet, and organization \
             items must be in a collection.",
            count(report.items_left_out, "item")
        ));
    }
    if report.attachments_skipped > 0 {
        warnings.push(format!(
            "Skipped {}: the account has no premium subscription, which Bitwarden requires \
             for attachments.",
            count(report.attachments_skipped, "attachment")
        ));
    }
    if report.attachments_missing > 0 {
        warnings.push(format!(
            "The backup lacks the content of {} listed in the snapshot, because it was made \
             with BACKUP_ATTACHMENTS=false.",
            count(report.attachments_missing, "attachment")
        ));
    }
    Ok(PreparedRestore {
        target,
        snapshot,
        existing,
        items: prepared,
        report,
        warnings,
        concurrency: options.concurrency.max(1),
    })
}

/// Checks what [`crate::snapshot::parse`] ensures, for a snapshot made some
/// other way: every id is unique, every item is usable, and everything an
/// item refers to is in the snapshot.
fn check_input(snapshot: &Restorable) -> Result<()> {
    fn unique<'a>(what: &str, ids: impl Iterator<Item = &'a str>) -> Result<HashSet<&'a str>> {
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id) {
                bail!("the snapshot holds {what} {id} more than once");
            }
        }
        Ok(seen)
    }
    let folders = unique("folder", snapshot.folders.iter().map(|f| f.id.as_str()))?;
    let collections = unique(
        "collection",
        snapshot.collections.iter().map(|c| c.id.as_str()),
    )?;
    unique("item", snapshot.items.iter().map(|i| i.id.as_str()))?;
    let attachments = snapshot.items.iter().flat_map(|item| &item.attachments);
    unique("attachment", attachments.map(|a| a.attachment_id.as_str()))?;
    for item in &snapshot.items {
        let usable = matches!(item.content.get("type"), Some(Node::Number(n)) if n.is_u64())
            && matches!(
                item.content.get("name"),
                Some(Node::Secret(_) | Node::Text(_))
            );
        let listed = item
            .folder_id
            .iter()
            .all(|id| folders.contains(id.as_str()))
            && (item.collection_ids.iter()).all(|id| collections.contains(id.as_str()));
        let own = item.attachments.iter().all(|a| a.item_id == item.id);
        if !(usable && listed && own) {
            bail!(
                "snapshot item {} has no item type or name, or refers to what the snapshot \
                 does not hold",
                item.id
            );
        }
    }
    Ok(())
}

/// Copies an attachment file out of the backup and checks that it decrypts.
/// Only its length and digest stay in memory.
fn stage<'a>(
    store: Option<&'a Store<'a>>,
    attachment: &'a SnapshotAttachment,
) -> Result<StagedFile<'a>> {
    let store = store.ok_or_else(|| anyhow!("no backup directory was given to read it from"))?;
    let staged = store.stage_snapshot_attachment(attachment)?;
    let data = staged.read_plaintext()?;
    Ok(StagedFile {
        attachment,
        length: data.len(),
        sha256: Sha256::digest(&data).into(),
        staged,
    })
}

#[derive(Clone)]
struct RestoredFile {
    attachment_id: String,
    file_name: String,
    length: usize,
    sha256: [u8; 32],
}

#[derive(Default)]
struct ExpectedContainers {
    folders: Vec<Folder>,
    collections: Vec<Collection>,
}

struct ExpectedRestoredItem {
    item: Item,
    files: Vec<RestoredFile>,
}

impl PreparedRestore<'_> {
    pub async fn apply(self, reporter: &dyn Reporter) -> Result<RestoreReport> {
        let Self {
            target,
            snapshot,
            existing,
            items,
            mut report,
            warnings,
            concurrency,
        } = self;
        for warning in &warnings {
            reporter.warn(warning);
        }
        let account = target.account;
        let scope = &target.scope;

        // Snapshot container id -> target container id, matched by name, and
        // the containers as the target must show them afterwards.
        let mut folder_ids: HashMap<&str, String> = HashMap::new();
        let mut collection_ids: HashMap<&str, String> = HashMap::new();
        let mut containers = ExpectedContainers::default();
        match scope {
            Scope::Personal => {
                let mut by_name: HashMap<String, Folder> = (existing.folders.iter())
                    .map(|f| (f.name.clone(), f.clone()))
                    .collect();
                for folder in &snapshot.folders {
                    let mapped = match by_name.get(&folder.name) {
                        Some(found) => found.clone(),
                        None => {
                            report.folders_created += 1;
                            let created = account.create_folder(&folder.name).await?;
                            by_name.insert(created.name.clone(), created.clone());
                            created
                        }
                    };
                    folder_ids.insert(&folder.id, mapped.id.clone());
                    // Folders of the same name in the snapshot share one.
                    if !containers.folders.contains(&mapped) {
                        containers.folders.push(mapped);
                    }
                }
            }
            Scope::Organization(org) => {
                let mut by_name: HashMap<String, Collection> = (existing.collections.iter())
                    .map(|c| (c.name.clone(), c.clone()))
                    .collect();
                for collection in &snapshot.collections {
                    let mapped = match by_name.get(&collection.name) {
                        Some(found) => found.clone(),
                        None => {
                            report.collections_created += 1;
                            let created = account
                                .create_collection(
                                    org,
                                    &collection.name,
                                    collection.external_id.as_deref(),
                                )
                                .await?;
                            by_name.insert(created.name.clone(), created.clone());
                            created
                        }
                    };
                    collection_ids.insert(&collection.id, mapped.id.clone());
                    if !containers.collections.contains(&mapped) {
                        containers.collections.push(mapped);
                    }
                }
            }
        }

        let drafts: Vec<Draft> = items
            .iter()
            .map(|(item, _)| {
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
        let writes = Tally::new(reporter, "Restoring", drafts.len(), "items");
        let created = account
            .create_items(scope, &drafts, &folder_names, concurrency, &|n| {
                writes.add(n)
            })
            .await?;
        check_created(drafts.len(), &created, &existing)?;

        let files: Vec<Vec<RestoredFile>> = stream::iter(items.iter().zip(&created).map(
            |((item, files), created)| async move {
                let mut restored = Vec::new();
                for file in files {
                    let uploaded = upload(target, created, file).await.with_context(|| {
                        format!(
                            "restoring attachment {} of snapshot item {}",
                            file.attachment.attachment_id, item.id
                        )
                    })?;
                    restored.push(uploaded);
                }
                anyhow::Ok(restored)
            },
        ))
        .buffered(concurrency)
        .try_collect()
        .await?;

        let archived: Vec<String> = items
            .iter()
            .zip(&created)
            .filter(|((item, _), _)| item.archived)
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

        let expected: Vec<ExpectedRestoredItem> =
            (items.iter().zip(&drafts).zip(created).zip(files))
                .map(
                    |((((item, _), draft), created), files)| ExpectedRestoredItem {
                        item: Item {
                            folder_id: draft.folder_id.clone(),
                            collection_ids: draft.collection_ids.clone(),
                            archived: item.archived && !archive_refused,
                            content: draft.content.clone(),
                            attachments: Vec::new(),
                            ..created
                        },
                        files,
                    },
                )
                .collect();
        verify(target, &containers, &expected, concurrency).await?;
        report.items = expected.len();
        report.attachments_restored = expected.iter().map(|e| e.files.len()).sum();
        Ok(report)
    }
}

/// Uploads a staged file from the same staged copy that was checked.
async fn upload(target: &Endpoint<'_>, item: &Item, file: &StagedFile<'_>) -> Result<RestoredFile> {
    let data = file.staged.read_plaintext()?;
    let sha256: [u8; 32] = Sha256::digest(&data).into();
    if (data.len(), sha256) != (file.length, file.sha256) {
        bail!("the copy staged from the backup changed since it was checked");
    }
    let attachment_id = target
        .account
        .upload_attachment(&target.scope, item, &file.attachment.file_name, &data)
        .await?;
    Ok(RestoredFile {
        attachment_id,
        file_name: file.attachment.file_name.clone(),
        length: file.length,
        sha256,
    })
}

/// Fails unless the server reported one new item for every one it was sent.
fn check_created(sent: usize, created: &[Item], existing: &Vault) -> Result<()> {
    if created.len() != sent {
        bail!(
            "the server reported {} after being sent {}",
            count(created.len(), "new item"),
            count(sent, "item")
        );
    }
    let before: HashSet<&str> = existing.items.iter().map(|i| i.id.as_str()).collect();
    let mut seen = HashSet::new();
    for item in created {
        if before.contains(item.id.as_str()) || !seen.insert(item.id.as_str()) {
            bail!(
                "the server reported item {} as new more than once, or it was there before",
                item.id
            );
        }
    }
    Ok(())
}

/// Reads the target vault again and checks every restored item: content,
/// folder or collections, archive state, and each attachment's id, name and
/// content.
async fn verify(
    target: &Endpoint<'_>,
    containers: &ExpectedContainers,
    expected: &[ExpectedRestoredItem],
    concurrency: usize,
) -> Result<()> {
    let vault = target
        .account
        .load(&target.scope)
        .await
        .context("reading the target vault again")?;
    let mode = match target.scope {
        Scope::Personal => Mode::Personal,
        Scope::Organization(_) => Mode::Organization,
    };
    let mut problems = container_differences(containers, &vault);
    problems.extend(differences(expected, &vault, mode));
    if problems.is_empty() {
        let by_id: HashMap<&str, &Item> = (vault.items.iter())
            .map(|item| (item.id.as_str(), item))
            .collect();
        let downloads = expected.iter().flat_map(|wanted| {
            let item = by_id[wanted.item.id.as_str()];
            wanted.files.iter().map(move |file| (item, file))
        });
        problems = stream::iter(downloads.map(|(item, file)| async move {
            // `differences` found every uploaded attachment on its item.
            let attachment = (item.attachments.iter())
                .find(|a| a.id == file.attachment_id)
                .ok_or_else(|| anyhow!("attachment {} is gone", file.attachment_id))?;
            let data = target
                .account
                .download_attachment(&target.scope, item, attachment)
                .await
                .with_context(|| {
                    format!(
                        "reading attachment {} of restored item {} back",
                        attachment.id, item.id
                    )
                })?;
            let sha256: [u8; 32] = Sha256::digest(&data).into();
            anyhow::Ok(
                ((data.len(), sha256) != (file.length, file.sha256)).then(|| {
                    format!(
                        "item {}: attachment {} holds other content than was uploaded",
                        item.id, attachment.id
                    )
                }),
            )
        }))
        .buffer_unordered(concurrency)
        .try_filter_map(|problem| async move { Ok(problem) })
        .try_collect()
        .await?;
    }
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
    Ok(())
}

/// What differs between the folders and collections as the restore created
/// or found them and `vault`. A server can accept a new name but keep
/// another, and the items in it would still look right.
fn container_differences(containers: &ExpectedContainers, vault: &Vault) -> Vec<String> {
    let mut problems = Vec::new();
    for wanted in &containers.folders {
        match vault.folders.iter().find(|found| found.id == wanted.id) {
            None => problems.push(format!(
                "folder {}: the target vault does not show it",
                wanted.id
            )),
            Some(found) if found.name != wanted.name => {
                problems.push(format!("folder {}: its name differs", wanted.id))
            }
            Some(_) => {}
        }
    }
    for wanted in &containers.collections {
        match vault.collections.iter().find(|found| found.id == wanted.id) {
            None => problems.push(format!(
                "collection {}: the target vault does not show it",
                wanted.id
            )),
            Some(found) if found.name != wanted.name || found.external_id != wanted.external_id => {
                problems.push(format!(
                    "collection {}: its name or external id differs",
                    wanted.id
                ))
            }
            Some(_) => {}
        }
    }
    problems
}

/// What differs between the restored items as written and `vault`, item by
/// item, as paths and ids only: this ends up in logs.
fn differences(expected: &[ExpectedRestoredItem], vault: &Vault, mode: Mode) -> Vec<String> {
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
    for wanted in expected {
        let id = &wanted.item.id;
        let Some(found) = by_id.get(id.as_str()) else {
            problems.push(format!("item {id}: the target vault does not show it"));
            continue;
        };
        let mut reasons = Vec::new();
        let paths = differing_paths(&shape(&wanted.item), &shape(found));
        if !paths.is_empty() {
            reasons.push(format!("it differs at [{}]", paths.join(", ")));
        }
        if found.archived != wanted.item.archived {
            reasons.push("its archive state differs".to_string());
        }
        let mut uploaded: Vec<(&str, &str)> = (wanted.files.iter())
            .map(|f| (f.attachment_id.as_str(), f.file_name.as_str()))
            .collect();
        let mut stored: Vec<(&str, &str)> = (found.attachments.iter())
            .map(|a| (a.id.as_str(), a.file_name.as_str()))
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

pub async fn restore(
    target: &Endpoint<'_>,
    snapshot: &Restorable,
    store: Option<&Store<'_>>,
    options: &RestoreOptions,
    reporter: &dyn Reporter,
) -> Result<RestoreReport> {
    prepare(target, snapshot, store, options)
        .await?
        .apply(reporter)
        .await
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

    fn files() -> Vec<RestoredFile> {
        ["a1", "a2"]
            .map(|id| RestoredFile {
                attachment_id: id.to_string(),
                file_name: "scan.pdf".to_string(),
                length: 1,
                sha256: [0; 32],
            })
            .to_vec()
    }

    fn expected() -> ExpectedRestoredItem {
        ExpectedRestoredItem {
            item: note("i"),
            files: files(),
        }
    }

    /// The restored item as a server that kept everything shows it.
    fn kept() -> Item {
        let mut item = note("i");
        item.attachments = (files().into_iter())
            .map(|file| Attachment {
                id: file.attachment_id,
                file_name: file.file_name,
                size: 100,
                wrapped_key: None,
            })
            .collect();
        item
    }

    fn check(found: Item, mode: Mode) -> Vec<String> {
        let vault = Vault {
            items: vec![found],
            ..Vault::default()
        };
        differences(&[expected()], &vault, mode)
    }

    #[test]
    fn an_item_that_kept_everything_passes_whatever_the_server_adds() {
        let mut found = kept();
        // Servers return empty lists for what was left out, and their own dates.
        found
            .content
            .insert("fields".into(), Node::List(Vec::new()));
        found.revision_date = Some("2026-10-16T12:00:00.000Z".into());
        assert_eq!(check(found, Mode::Personal), Vec::<String>::new());
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
                "file under another id",
                |item| item.attachments[1].id = "a3".into(),
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
            let problems = check(found, Mode::Personal);
            assert_eq!(problems.len(), 1, "{what}");
            assert!(problems[0].starts_with("item i: "), "{what}");
            assert!(problems[0].contains(reason), "{what}: {}", problems[0]);
            assert!(!problems[0].contains("written down"), "{what}");
        }
    }

    #[test]
    fn organization_items_are_checked_by_collection_without_favorites() {
        let mut found = kept();
        found.collection_ids = vec!["c1".into()];
        let mut expected = expected();
        expected.item.collection_ids = vec!["c2".into(), "c1".into()];
        let vault = |found: &Item| Vault {
            items: vec![found.clone()],
            ..Vault::default()
        };
        let problems = differences(&[expected], &vault(&found), Mode::Organization);
        assert!(
            problems[0].contains("differs at [collections]"),
            "{problems:?}"
        );

        let mut expected = self::expected();
        expected.item.collection_ids = vec!["c2".into(), "c1".into()];
        found.collection_ids = vec!["c1".into(), "c2".into()];
        // Bitwarden keeps an organization item's favorite flag per member.
        found.content.insert("favorite".into(), Node::Bool(true));
        assert_eq!(
            differences(&[expected], &vault(&found), Mode::Organization),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_container_the_server_named_otherwise_fails() {
        let containers = ExpectedContainers {
            folders: vec![Folder {
                id: "f".into(),
                name: "Work".into(),
            }],
            collections: vec![Collection {
                id: "c".into(),
                name: "".into(),
                external_id: Some("ext".into()),
            }],
        };
        let mut vault = Vault {
            folders: containers.folders.clone(),
            collections: containers.collections.clone(),
            ..Vault::default()
        };
        assert_eq!(
            container_differences(&containers, &vault),
            Vec::<String>::new()
        );
        vault.folders[0].name = String::new();
        vault.collections[0].external_id = None;
        assert_eq!(
            container_differences(&containers, &vault),
            [
                "folder f: its name differs",
                "collection c: its name or external id differs"
            ]
        );
        vault.folders.clear();
        vault.collections[0] = containers.collections[0].clone();
        assert_eq!(
            container_differences(&containers, &vault),
            ["folder f: the target vault does not show it"]
        );
    }

    #[test]
    fn hand_made_snapshots_are_checked_like_parsed_ones() {
        let item = |id: &str| RestorableItem {
            id: id.into(),
            folder_id: None,
            collection_ids: Vec::new(),
            archived: false,
            content: note(id).content,
            attachments: Vec::new(),
        };
        let valid = Restorable {
            items: vec![item("a"), item("b")],
            ..Restorable::default()
        };
        assert!(check_input(&valid).is_ok());
        type Edit = (&'static str, fn(&mut Restorable));
        let edits: Vec<Edit> = vec![
            ("same item twice", |s| s.items[1].id = "a".into()),
            ("folder not listed", |s| {
                s.items[0].folder_id = Some("f".into())
            }),
            ("collection not listed", |s| {
                s.items[0].collection_ids = vec!["c".into()]
            }),
            ("no type", |s| _ = s.items[0].content.remove("type")),
            ("no name", |s| _ = s.items[0].content.remove("name")),
            ("another item's attachment", |s| {
                s.items[0].attachments.push(SnapshotAttachment {
                    item_id: "b".into(),
                    attachment_id: "x".into(),
                    file_name: "f".into(),
                    size: 1,
                    storage: AttachmentStorage::Omitted,
                })
            }),
        ];
        for (what, edit) in edits {
            let mut snapshot = valid.clone();
            edit(&mut snapshot);
            assert!(check_input(&snapshot).is_err(), "{what}");
        }
    }

    #[test]
    fn the_server_must_report_each_new_item_once() {
        let none = Vault::default();
        assert!(check_created(2, &[note("a"), note("b")], &none).is_ok());
        assert!(check_created(2, &[note("a")], &none).is_err());
        assert!(check_created(2, &[note("a"), note("a")], &none).is_err());
        let before = Vault {
            items: vec![note("old")],
            ..Vault::default()
        };
        assert!(check_created(1, &[note("old")], &before).is_err());
    }

    #[test]
    fn a_report_with_anything_left_out_has_warned() {
        assert!(!RestoreReport::default().warned());
        for report in [
            RestoreReport {
                items_left_out: 1,
                ..RestoreReport::default()
            },
            RestoreReport {
                items_left_unarchived: 1,
                ..RestoreReport::default()
            },
            RestoreReport {
                attachments_missing: 1,
                ..RestoreReport::default()
            },
            RestoreReport {
                attachments_skipped: 1,
                ..RestoreReport::default()
            },
        ] {
            assert!(report.warned(), "{report:?}");
        }
    }
}
