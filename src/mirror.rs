//! Makes a destination vault match a source vault.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use sha2::{Digest, Sha256};

use crate::{
    plan::{self, Mode, Plan, Safety, Summary, comparable, differing_paths, marker_of},
    vault::{Account, Item, Scope, ServerTraits, Vault},
};

/// Receives progress lines. Messages never contain vault content, only
/// counts and server-side ids.
pub trait Reporter: Sync {
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
}

#[derive(Debug, Clone)]
pub struct MirrorOptions {
    /// Work out and report the plan, but change nothing.
    pub dry_run: bool,
    pub safety: Safety,
    /// How many item writes run at once.
    pub concurrency: usize,
    /// Download every copied attachment from the destination again and
    /// compare it with what was read from the source.
    pub verify_attachments: bool,
    /// The folders, or in an organization the collections, to mirror.
    pub filter: Filter,
}

/// Names of folders, or of collections in an organization, that select what
/// is mirrored. A name also matches everything nested under it, so `Work`
/// matches `Work/Infra`. Exclusion wins over inclusion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// When not empty, only these are mirrored, and items in no folder or
    /// collection are not.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    fn matches(pattern: &str, name: &str) -> bool {
        name.strip_prefix(pattern)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }

    fn excludes(&self, name: &str) -> bool {
        self.exclude.iter().any(|p| Self::matches(p, name))
    }

    fn keeps(&self, name: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|p| Self::matches(p, name)))
            && !self.excludes(name)
    }

    /// Reduces a source vault to what the filter selects. An organization
    /// item in an excluded collection is left out even when it is also in an
    /// included one; otherwise it keeps only its selected collections.
    fn apply(&self, vault: &mut Vault, mode: Mode) {
        let unfiled = self.include.is_empty();
        match mode {
            Mode::Personal => {
                vault.folders.retain(|folder| self.keeps(&folder.name));
                let kept: HashSet<&str> = vault.folders.iter().map(|f| f.id.as_str()).collect();
                vault.items.retain(|item| match &item.folder_id {
                    Some(id) => kept.contains(id.as_str()),
                    None => unfiled,
                });
            }
            Mode::Organization => {
                let excluded: HashSet<String> = vault
                    .collections
                    .iter()
                    .filter(|c| self.excludes(&c.name))
                    .map(|c| c.id.clone())
                    .collect();
                vault.collections.retain(|c| self.keeps(&c.name));
                let kept: HashSet<&str> = vault.collections.iter().map(|c| c.id.as_str()).collect();
                vault.items.retain_mut(|item| {
                    if item.collection_ids.is_empty() {
                        return unfiled;
                    }
                    if item.collection_ids.iter().any(|id| excluded.contains(id)) {
                        return false;
                    }
                    item.collection_ids.retain(|id| kept.contains(id.as_str()));
                    !item.collection_ids.is_empty()
                });
            }
        }
    }
}

impl Default for MirrorOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            safety: Safety::default(),
            concurrency: 4,
            verify_attachments: true,
            filter: Filter::default(),
        }
    }
}

/// One side of a mirror: an unlocked account and the part of it to work on.
pub struct Endpoint<'a> {
    pub account: &'a Account,
    pub scope: Scope,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    pub summary: Summary,
    /// False for a dry run and for a run that found nothing to do.
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// A run that failed, with the phase it failed in for the status file.
#[derive(Debug, thiserror::Error)]
#[error("{stage} failed")]
pub struct MirrorError {
    pub stage: &'static str,
    /// What the plan intended, when the run got far enough to have one.
    pub summary: Option<Summary>,
    #[source]
    pub source: anyhow::Error,
}

/// A copied attachment, remembered so the destination can be checked.
struct Copied {
    source_id: String,
    file_name: String,
    sha256: [u8; 32],
}

pub async fn mirror(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    options: &MirrorOptions,
    reporter: &dyn Reporter,
) -> Result<Outcome, MirrorError> {
    let mut run = Run {
        source,
        dest,
        options,
        reporter,
        stage: "reading the vaults",
        summary: None,
        warnings: Vec::new(),
        archive_unsupported: false,
    };
    match run.execute().await {
        Ok(changed) => Ok(Outcome {
            summary: run.summary.unwrap_or_default(),
            changed,
            warnings: run.warnings,
        }),
        Err(error) => Err(MirrorError {
            stage: run.stage,
            summary: run.summary,
            source: error,
        }),
    }
}

struct Run<'a> {
    source: &'a Endpoint<'a>,
    dest: &'a Endpoint<'a>,
    options: &'a MirrorOptions,
    reporter: &'a dyn Reporter,
    stage: &'static str,
    summary: Option<Summary>,
    warnings: Vec<String>,
    /// Set when the destination server refuses to archive items.
    archive_unsupported: bool,
}

impl Run<'_> {
    fn mode(&self) -> Result<Mode> {
        match (&self.source.scope, &self.dest.scope) {
            (Scope::Personal, Scope::Personal) => Ok(Mode::Personal),
            (Scope::Organization(_), Scope::Organization(_)) => Ok(Mode::Organization),
            _ => bail!(
                "the source and destination must both be individual vaults or both be organizations"
            ),
        }
    }

    /// Removes source items the destination could not hold, with a warning.
    fn leave_out(&mut self, source: &mut Vault, unfit: impl Fn(&Item) -> bool, reason: &str) {
        let count = source
            .items
            .iter()
            .filter(|item| !item.in_trash && unfit(item))
            .count();
        source.items.retain(|item| !unfit(item));
        if count > 0 {
            self.warn(format!(
                "{count} item(s) are left out of the mirror because {reason}."
            ));
        }
    }

    fn filter(&mut self, source: &mut Vault, mode: Mode) {
        let options = self.options;
        let filter = &options.filter;
        if filter.is_empty() {
            return;
        }
        let names: Vec<&str> = match mode {
            Mode::Personal => source.folders.iter().map(|f| f.name.as_str()).collect(),
            Mode::Organization => source.collections.iter().map(|c| c.name.as_str()).collect(),
        };
        let kind = match mode {
            Mode::Personal => "folder",
            Mode::Organization => "collection",
        };
        // A misspelt name would otherwise select nothing without a word.
        let unmatched: Vec<String> = filter
            .include
            .iter()
            .chain(&filter.exclude)
            .filter(|pattern| !names.iter().any(|name| Filter::matches(pattern, name)))
            .map(|pattern| format!("{pattern:?}"))
            .collect();
        let before = source.items.iter().filter(|item| !item.in_trash).count();
        filter.apply(source, mode);
        let after = source.items.iter().filter(|item| !item.in_trash).count();
        if !unmatched.is_empty() {
            self.warn(format!(
                "The filter names {}, which match(es) no {kind} in the source.",
                unmatched.join(", ")
            ));
        }
        self.reporter.info(&format!(
            "The {kind} filter selects {after} of {before} item(s)."
        ));
    }

    fn warn(&mut self, message: String) {
        self.reporter.warn(&message);
        self.warnings.push(message);
    }

    async fn execute(&mut self) -> Result<bool> {
        let mode = self.mode()?;
        if self.source.account.api_url() == self.dest.account.api_url()
            && self.source.account.user_id == self.dest.account.user_id
            && self.source.scope == self.dest.scope
        {
            bail!("the source and destination are the same vault");
        }

        let (source_vault, dest_vault) = tokio::try_join!(
            async {
                self.source
                    .account
                    .load(&self.source.scope)
                    .await
                    .context("reading the source vault")
            },
            async {
                self.dest
                    .account
                    .load(&self.dest.scope)
                    .await
                    .context("reading the destination vault")
            },
        )?;

        let mut source_vault = source_vault;
        self.filter(&mut source_vault, mode);
        let traits = match self.dest.account.server_traits().await {
            Ok(traits) => traits,
            Err(error) => {
                self.warn(format!(
                    "Could not read the destination's settings, so every item is mirrored: {error:#}"
                ));
                ServerTraits {
                    hidden_item_types: Vec::new(),
                    items_need_collections: false,
                }
            }
        };
        self.leave_out(
            &mut source_vault,
            |item| {
                item.item_type()
                    .is_some_and(|kind| traits.hidden_item_types.contains(&kind))
            },
            "the destination server does not show bank accounts, driver's licences or passports yet",
        );
        if mode == Mode::Personal && !self.dest.account.premium {
            let count: usize = source_vault
                .items
                .iter()
                .filter(|item| !item.in_trash)
                .map(|item| item.attachments.len())
                .sum();
            if count > 0 {
                for item in &mut source_vault.items {
                    item.attachments.clear();
                }
                self.warn(format!(
                    "{count} attachment(s) are not copied because the destination account has no \
                     premium subscription, which Bitwarden requires for attachments. They are \
                     copied once it has one."
                ));
            }
        }
        if mode == Mode::Organization && traits.items_need_collections {
            self.leave_out(
                &mut source_vault,
                |item| item.collection_ids.is_empty(),
                "they are in no collection, which the destination server requires",
            );
        }

        self.stage = "planning";
        let plan = plan::plan(&source_vault, &dest_vault, mode);
        let summary = plan.summary(&source_vault);
        self.summary = Some(summary);
        self.reporter.info(&format!(
            "{}: {}",
            if self.options.dry_run {
                "Dry-run plan"
            } else {
                "Plan"
            },
            describe_plan(&summary)
        ));
        plan::check_safety(&plan, &self.options.safety)?;
        if self.options.dry_run || !plan.has_changes() {
            return Ok(false);
        }

        let copied = self.apply(&plan, &source_vault, &dest_vault).await?;

        self.stage = "verifying the destination";
        self.verify(&source_vault, mode, &copied).await?;
        self.reporter.info("Destination verified.");
        Ok(true)
    }

    async fn apply(
        &mut self,
        plan: &Plan,
        source_vault: &Vault,
        dest_vault: &Vault,
    ) -> Result<Vec<Copied>> {
        let source = self.source;
        let dest = self.dest;
        let concurrency = self.options.concurrency.max(1);

        self.stage = "creating folders and collections";
        let doomed_folders: Vec<&str> = plan
            .folders_to_delete
            .iter()
            .map(|f| f.id.as_str())
            .collect();
        let mut folder_ids: HashMap<String, String> = dest_vault
            .folders
            .iter()
            .filter(|f| !doomed_folders.contains(&f.id.as_str()))
            .map(|f| (f.name.clone(), f.id.clone()))
            .collect();
        for name in &plan.folders_to_create {
            let folder = dest.account.create_folder(name).await?;
            folder_ids.insert(folder.name, folder.id);
        }
        let doomed_collections: Vec<&str> = plan
            .collections_to_delete
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        let mut collection_ids: HashMap<String, String> = dest_vault
            .collections
            .iter()
            .filter(|c| !doomed_collections.contains(&c.id.as_str()))
            .map(|c| (c.name.clone(), c.id.clone()))
            .collect();
        if let Scope::Organization(org) = &dest.scope {
            for new in &plan.collections_to_create {
                let collection = dest
                    .account
                    .create_collection(org, &new.name, new.external_id.as_deref())
                    .await?;
                collection_ids.insert(collection.name, collection.id);
            }
        }

        self.stage = "writing items";
        // Each write yields the entry it was for and the destination item as
        // the server now holds it.
        let drafts = plan
            .creates
            .iter()
            .map(|&entry_index| {
                plan.draft(
                    &plan.entries[entry_index],
                    source_vault,
                    &folder_ids,
                    &collection_ids,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let folder_names: HashMap<String, String> = folder_ids
            .iter()
            .map(|(name, id)| (id.clone(), name.clone()))
            .collect();
        let new_items = dest
            .account
            .create_items(&dest.scope, &drafts, &folder_names, concurrency)
            .await?;
        let created: Vec<(usize, Item)> = plan.creates.iter().copied().zip(new_items).collect();
        if !created.is_empty() {
            self.reporter
                .info(&format!("Created {} item(s).", created.len()));
        }

        let updated: Vec<(usize, Item)> =
            stream::iter(plan.updates.iter().enumerate().map(|(index, update)| {
                let (folder_ids, collection_ids) = (&folder_ids, &collection_ids);
                async move {
                    let existing = &dest_vault.items[update.dest];
                    if !update.write_content {
                        return anyhow::Ok((index, existing.clone()));
                    }
                    let draft = plan.draft(
                        &plan.entries[update.entry],
                        source_vault,
                        folder_ids,
                        collection_ids,
                    )?;
                    let item = dest
                        .account
                        .update_item(&dest.scope, existing, &draft)
                        .await
                        .with_context(|| format!("updating destination item {}", existing.id))?;
                    Ok((index, item))
                }
            }))
            .buffer_unordered(concurrency)
            .try_collect()
            .await?;
        let rewritten = plan
            .updates
            .iter()
            .filter(|update| update.write_content)
            .count();
        if rewritten > 0 {
            self.reporter.info(&format!("Updated {rewritten} item(s)."));
        }

        self.stage = "setting archive state";
        let mut to_archive: Vec<String> = Vec::new();
        let mut to_unarchive: Vec<String> = Vec::new();
        // Decided from what the server reports after the writes, not from the
        // plan: servers differ in what a write does to an item's archive state.
        let written = created
            .iter()
            .map(|(entry_index, item)| (*entry_index, item))
            .chain(
                updated
                    .iter()
                    .map(|(index, item)| (plan.updates[*index].entry, item)),
            );
        for (entry_index, item) in written {
            let wanted = source_vault.items[plan.entries[entry_index].source].archived;
            match (wanted, item.archived) {
                (true, false) => to_archive.push(item.id.clone()),
                (false, true) => to_unarchive.push(item.id.clone()),
                _ => {}
            }
        }
        for (ids, archived) in [(to_archive, true), (to_unarchive, false)] {
            if ids.is_empty() {
                continue;
            }
            if let Err(error) = dest.account.set_archived(&ids, archived).await {
                // Archiving is a paid feature on some servers. The items are
                // mirrored either way, so this does not fail the run.
                self.archive_unsupported = true;
                self.warn(format!(
                    "The destination refused to change the archive state of {} item(s): {error:#}",
                    ids.len()
                ));
            }
        }

        self.stage = "copying attachments";
        // (source item, destination item, destination attachments to remove first)
        let mut transfers: Vec<(&Item, &Item, Vec<String>)> = Vec::new();
        for (entry_index, item) in &created {
            let source_item = &source_vault.items[plan.entries[*entry_index].source];
            if !source_item.attachments.is_empty() {
                transfers.push((source_item, item, Vec::new()));
            }
        }
        for (index, item) in &updated {
            let update = &plan.updates[*index];
            if update.sync_attachments {
                let source_item = &source_vault.items[plan.entries[update.entry].source];
                let old = dest_vault.items[update.dest]
                    .attachments
                    .iter()
                    .map(|a| a.id.clone())
                    .collect();
                transfers.push((source_item, item, old));
            }
        }
        let copied: Vec<Vec<Copied>> = stream::iter(transfers.into_iter().map(
            |(source_item, dest_item, old)| async move {
                for attachment_id in &old {
                    dest.account
                        .delete_attachment(&dest_item.id, attachment_id)
                        .await?;
                }
                let mut copied = Vec::new();
                for attachment in &source_item.attachments {
                    let data = source
                        .account
                        .download_attachment(&source.scope, source_item, attachment)
                        .await
                        .with_context(|| {
                            format!(
                                "downloading attachment {} of source item {}",
                                attachment.id, source_item.id
                            )
                        })?;
                    dest.account
                        .upload_attachment(&dest.scope, dest_item, &attachment.file_name, &data)
                        .await
                        .with_context(|| {
                            format!(
                                "uploading an attachment to destination item {}",
                                dest_item.id
                            )
                        })?;
                    copied.push(Copied {
                        source_id: source_item.id.clone(),
                        file_name: attachment.file_name.clone(),
                        sha256: Sha256::digest(&data).into(),
                    });
                }
                anyhow::Ok(copied)
            },
        ))
        // Attachments are held in memory while they are re-encrypted.
        .buffer_unordered(concurrency.min(2))
        .try_collect()
        .await?;
        let copied: Vec<Copied> = copied.into_iter().flatten().collect();
        if !copied.is_empty() {
            self.reporter
                .info(&format!("Copied {} attachment(s).", copied.len()));
        }

        // Deletions come last: until here a failed run has removed nothing.
        self.stage = "deleting obsolete items";
        let ids: Vec<String> = plan
            .deletes
            .iter()
            .map(|delete| dest_vault.items[delete.dest].id.clone())
            .collect();
        if !ids.is_empty() {
            dest.account.delete_items(&ids).await?;
            self.reporter
                .info(&format!("Deleted {} item(s).", ids.len()));
        }
        for folder in &plan.folders_to_delete {
            dest.account.delete_folder(&folder.id).await?;
        }
        if let Scope::Organization(org) = &dest.scope {
            for collection in &plan.collections_to_delete {
                dest.account.delete_collection(org, &collection.id).await?;
            }
        }
        Ok(copied)
    }

    /// Reads the destination again and checks that nothing is left to do.
    async fn verify(&mut self, source_vault: &Vault, mode: Mode, copied: &[Copied]) -> Result<()> {
        let dest = self.dest;
        let dest_vault = dest.account.load(&dest.scope).await?;
        let mut again = plan::plan(source_vault, &dest_vault, mode);
        if self.archive_unsupported {
            again
                .updates
                .retain(|update| update.write_content || update.sync_attachments);
        }
        if again.has_changes() {
            let mut details = Vec::new();
            for update in again.updates.iter().take(5) {
                let entry = &again.entries[update.entry];
                let wanted = comparable(
                    &source_vault.items[entry.source],
                    entry.folder.as_deref(),
                    &entry.collections,
                    mode,
                );
                let dest_item = &dest_vault.items[update.dest];
                let folder = folder_name(&dest_vault, dest_item, mode);
                let collections = collection_names(&dest_vault, dest_item, mode);
                let found = comparable(dest_item, folder.as_deref(), &collections, mode);
                // Paths only, never values: this ends up in logs and the status file.
                let mut reasons = Vec::new();
                let paths = differing_paths(&wanted, &found);
                if !paths.is_empty() {
                    reasons.push(format!("content differs at [{}]", paths.join(", ")));
                } else if update.write_content {
                    reasons.push("its marker is out of date".to_string());
                }
                if update.sync_attachments {
                    reasons.push("its attachments differ".to_string());
                }
                if update.set_archived.is_some() {
                    reasons.push("its archive state differs".to_string());
                }
                details.push(format!("item {}: {}", dest_item.id, reasons.join(", ")));
            }
            let summary = again.summary(source_vault);
            bail!(
                "the destination did not converge: {} to create, {} to update, {} to delete{}{}",
                summary.items_created,
                summary.items_updated,
                summary.items_deleted,
                if details.is_empty() { "" } else { "; " },
                details.join("; ")
            );
        }

        if copied.is_empty() || !self.options.verify_attachments {
            return Ok(());
        }
        let mut expected: BTreeMap<&str, Vec<(&str, [u8; 32])>> = BTreeMap::new();
        for copy in copied {
            expected
                .entry(&copy.source_id)
                .or_default()
                .push((&copy.file_name, copy.sha256));
        }
        let by_source: HashMap<String, &Item> = dest_vault
            .items
            .iter()
            .filter_map(|item| Some((marker_of(item)?.source_id, item)))
            .collect();
        for (source_id, mut expected) in expected {
            let item = by_source
                .get(source_id)
                .ok_or_else(|| anyhow!("the mirror of source item {source_id} is missing"))?;
            let mut found = Vec::new();
            for attachment in &item.attachments {
                let data = dest
                    .account
                    .download_attachment(&dest.scope, item, attachment)
                    .await?;
                found.push((
                    attachment.file_name.as_str(),
                    <[u8; 32]>::from(Sha256::digest(&data)),
                ));
            }
            expected.sort_unstable();
            found.sort_unstable();
            if expected != found {
                bail!(
                    "the attachments of destination item {} do not match the source",
                    item.id
                );
            }
        }
        Ok(())
    }
}

fn folder_name(vault: &Vault, item: &Item, mode: Mode) -> Option<String> {
    if mode != Mode::Personal {
        return None;
    }
    let id = item.folder_id.as_deref()?;
    vault
        .folders
        .iter()
        .find(|folder| folder.id == id)
        .map(|folder| folder.name.clone())
}

fn collection_names(vault: &Vault, item: &Item, mode: Mode) -> Vec<String> {
    if mode != Mode::Organization {
        return Vec::new();
    }
    let mut names: Vec<String> = vault
        .collections
        .iter()
        .filter(|collection| item.collection_ids.contains(&collection.id))
        .map(|collection| collection.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The plan's changes in a few words, leaving out what does not happen.
fn describe_plan(summary: &Summary) -> String {
    let changes: Vec<String> = [
        (summary.items_created, "create", "item"),
        (summary.items_updated, "update", "item"),
        (summary.items_deleted, "delete", "item"),
        (summary.folders_created, "create", "folder"),
        (summary.folders_deleted, "delete", "folder"),
        (summary.collections_created, "create", "collection"),
        (summary.collections_deleted, "delete", "collection"),
        (summary.attachments_copied, "copy", "attachment"),
    ]
    .into_iter()
    .filter(|(count, ..)| *count > 0)
    .map(|(count, verb, noun)| {
        format!("{verb} {count} {noun}{}", if count == 1 { "" } else { "s" })
    })
    .collect();
    let unchanged = format!("{} item(s) unchanged", summary.items_unchanged);
    match changes.is_empty() {
        true => format!("nothing to change, {unchanged}"),
        false => format!("{}; {unchanged}", changes.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{Collection, Folder};

    fn item(id: &str, folder: Option<&str>, collections: &[&str]) -> Item {
        Item {
            id: id.into(),
            folder_id: folder.map(Into::into),
            collection_ids: collections.iter().map(|c| c.to_string()).collect(),
            revision_date: None,
            creation_date: None,
            in_trash: false,
            archived: false,
            archived_date: None,
            content: BTreeMap::new(),
            attachments: vec![],
            wrapped_key: None,
        }
    }

    fn filter(include: &[&str], exclude: &[&str]) -> Filter {
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect();
        Filter {
            include: names(include),
            exclude: names(exclude),
        }
    }

    fn ids(vault: &Vault) -> Vec<&str> {
        vault.items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn plans_are_described_by_what_changes() {
        let summary = Summary {
            items_created: 9,
            items_unchanged: 1,
            folders_created: 1,
            attachments_copied: 4,
            ..Summary::default()
        };
        assert_eq!(
            describe_plan(&summary),
            "create 9 items, create 1 folder, copy 4 attachments; 1 item(s) unchanged"
        );
        assert_eq!(
            describe_plan(&Summary {
                items_unchanged: 9,
                ..Summary::default()
            }),
            "nothing to change, 9 item(s) unchanged"
        );
    }

    #[test]
    fn folder_filters_select_folders_with_their_subfolders() {
        let vault = Vault {
            folders: ["Work", "Work/Infra", "Workshop", "Private"]
                .iter()
                .map(|name| Folder {
                    id: name.to_string(),
                    name: name.to_string(),
                })
                .collect(),
            collections: vec![],
            items: vec![
                item("work", Some("Work"), &[]),
                item("infra", Some("Work/Infra"), &[]),
                item("shop", Some("Workshop"), &[]),
                item("private", Some("Private"), &[]),
                item("unfiled", None, &[]),
            ],
        };
        let run = |filter: Filter| {
            let mut vault = vault.clone();
            filter.apply(&mut vault, Mode::Personal);
            let folders: Vec<String> = vault.folders.iter().map(|f| f.name.clone()).collect();
            (ids(&vault).join(" "), folders.join(" "))
        };
        assert_eq!(
            run(filter(&["Work"], &[])),
            ("work infra".into(), "Work Work/Infra".into())
        );
        assert_eq!(
            run(filter(&["Work"], &["Work/Infra"])),
            ("work".into(), "Work".into())
        );
        assert_eq!(
            run(filter(&[], &["Private"])),
            (
                "work infra shop unfiled".into(),
                "Work Work/Infra Workshop".into()
            )
        );
    }

    #[test]
    fn collection_filters_trim_collections_and_let_exclusion_win() {
        let vault = Vault {
            folders: vec![],
            collections: ["Shared", "Private", "Other"]
                .iter()
                .map(|name| Collection {
                    id: name.to_string(),
                    name: name.to_string(),
                    external_id: None,
                })
                .collect(),
            items: vec![
                item("shared", None, &["Shared", "Other"]),
                item("both", None, &["Shared", "Private"]),
                item("other", None, &["Other"]),
                item("loose", None, &[]),
            ],
        };
        let mut included = vault.clone();
        filter(&["Shared"], &["Private"]).apply(&mut included, Mode::Organization);
        assert_eq!(ids(&included), ["shared"]);
        assert_eq!(included.items[0].collection_ids, ["Shared"]);
        assert_eq!(included.collections.len(), 1);

        let mut excluded = vault.clone();
        filter(&[], &["Private"]).apply(&mut excluded, Mode::Organization);
        assert_eq!(ids(&excluded), ["shared", "other", "loose"]);
    }
}
