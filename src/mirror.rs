//! Makes a destination vault match a source vault.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use sha2::{Digest, Sha256};

use crate::{
    plan::{self, Mode, Plan, Safety, Summary, comparable, differing_paths, marker_of},
    vault::{Account, Item, Scope, Vault},
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
}

impl Default for MirrorOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            safety: Safety::default(),
            concurrency: 4,
            verify_attachments: true,
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
#[error("{stage} failed: {source:#}")]
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

        self.stage = "planning";
        let plan = plan::plan(&source_vault, &dest_vault, mode);
        let summary = plan.summary(&source_vault);
        self.summary = Some(summary);
        self.reporter.info(&format!(
            "{}: items create {} update {} delete {} unchanged {}; folders create {} delete {}; \
             collections create {} delete {}; attachments copy {}",
            if self.options.dry_run {
                "Dry-run plan"
            } else {
                "Plan"
            },
            summary.items_created,
            summary.items_updated,
            summary.items_deleted,
            summary.items_unchanged,
            summary.folders_created,
            summary.folders_deleted,
            summary.collections_created,
            summary.collections_deleted,
            summary.attachments_copied,
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
        let created: Vec<(usize, Item)> = stream::iter(plan.creates.iter().map(|&entry_index| {
            let (folder_ids, collection_ids) = (&folder_ids, &collection_ids);
            async move {
                let draft = plan.draft(
                    &plan.entries[entry_index],
                    source_vault,
                    folder_ids,
                    collection_ids,
                )?;
                let item = dest.account.create_item(&dest.scope, &draft).await?;
                anyhow::Ok((entry_index, item))
            }
        }))
        .buffer_unordered(concurrency)
        .try_collect()
        .await?;
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
        for (entry_index, item) in &created {
            if source_vault.items[plan.entries[*entry_index].source].archived {
                to_archive.push(item.id.clone());
            }
        }
        for (index, item) in &updated {
            match plan.updates[*index].set_archived {
                Some(true) => to_archive.push(item.id.clone()),
                Some(false) => to_unarchive.push(item.id.clone()),
                None => {}
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
                );
                let dest_item = &dest_vault.items[update.dest];
                let folder = folder_name(&dest_vault, dest_item, mode);
                let collections = collection_names(&dest_vault, dest_item, mode);
                let found = comparable(dest_item, folder.as_deref(), &collections);
                details.push(format!(
                    "item {} differs at [{}]",
                    dest_item.id,
                    differing_paths(&wanted, &found).join(", ")
                ));
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
