//! Makes a destination vault match a source vault.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use sha2::{Digest, Sha256};

use crate::{
    node::Node,
    plan::{
        self, DeleteReason, FileWrite, FolderCleanup, Mode, Plan, Receipt, Safety, Summary,
        comparable, differing_paths,
    },
    store::{FolderReceipts, Receipts, Snapshot},
    text::{count, listed},
    vault::{Account, Item, Scope, ServerTraits, Vault, VaultRead, WRITES_AT_ONCE},
};

/// Receives progress lines. Messages never contain vault content, only
/// counts and server-side ids.
pub trait Reporter: Sync {
    /// A step of the run, as a short verb, such as `Created`, and the rest
    /// of the sentence, such as `9 items`.
    fn status(&self, verb: &str, detail: &str);
    fn warn(&self, message: &str);
    /// `done` of `total` units of a long step, such as `Writing`, 3, 9,
    /// `items`. Only a terminal shows it; the step's status line follows.
    fn progress(&self, _verb: &str, _done: usize, _total: usize, _unit: &str) {}
}

/// Counts finished units of a step that runs concurrently and reports them.
pub(crate) struct Tally<'a> {
    reporter: &'a dyn Reporter,
    verb: &'static str,
    unit: &'static str,
    total: usize,
    done: AtomicUsize,
}

impl<'a> Tally<'a> {
    pub(crate) fn new(
        reporter: &'a dyn Reporter,
        verb: &'static str,
        total: usize,
        unit: &'static str,
    ) -> Self {
        Self {
            reporter,
            verb,
            unit,
            total,
            done: AtomicUsize::new(0),
        }
    }

    pub(crate) fn add(&self, n: usize) {
        let done = self.done.fetch_add(n, Ordering::Relaxed) + n;
        self.reporter
            .progress(self.verb, done, self.total, self.unit);
    }
}

#[derive(Debug, Clone, Default)]
pub struct MirrorOptions {
    pub safety: Safety,
    /// The folders, or in an organization the collections, to mirror.
    pub filter: Filter,
    /// Leave attachments out: copies keep theirs and new copies get none.
    pub skip_attachments: bool,
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

    pub(crate) fn matches(pattern: &str, name: &str) -> bool {
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

/// One side of a mirror: an unlocked account and the part of it to work on.
pub struct Endpoint<'a> {
    pub account: &'a Account,
    pub scope: Scope,
}

/// A vault as one read found it, and the snapshot that backs up exactly
/// that read. A run plans from it and copies attachment files out of it, so
/// it changes and copies only what a backup holds.
pub struct Protected<'a> {
    pub read: VaultRead,
    pub snapshot: Snapshot<'a>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    pub summary: Summary,
    /// False for a check and for a run that found nothing to do.
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

/// Makes the destination match the source, as both were read and backed up.
pub async fn mirror(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    source_read: &Protected<'_>,
    dest_read: &Protected<'_>,
    options: &MirrorOptions,
    folders: &FolderReceipts,
    reporter: &dyn Reporter,
) -> Result<Outcome, MirrorError> {
    let mut run = Run::new(source, dest, options, folders, reporter);
    let result = run
        .execute(
            &source_read.read,
            dest_read.read.vault(),
            Some(&source_read.snapshot),
        )
        .await;
    run.finish(result)
}

/// Works out and reports what a run would do, with the same checks, and
/// changes nothing.
pub async fn check(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    source_read: &VaultRead,
    dest_vault: &Vault,
    options: &MirrorOptions,
    folders: &FolderReceipts,
    reporter: &dyn Reporter,
) -> Result<Outcome, MirrorError> {
    let mut run = Run::new(source, dest, options, folders, reporter);
    let result = run.execute(source_read, dest_vault, None).await;
    run.finish(result)
}

/// Identifies one side for markers and folder receipts: its server, account
/// and scope.
fn vault_key(endpoint: &Endpoint<'_>) -> String {
    // Host names are case-insensitive, and a URL parses to one spelling.
    let url = reqwest::Url::parse(endpoint.account.api_url())
        .map_or_else(|_| endpoint.account.api_url().to_string(), String::from);
    let scope = match &endpoint.scope {
        Scope::Personal => "personal".to_string(),
        Scope::Organization(id) => format!("organization {id}"),
    };
    serde_json::json!([url.trim_end_matches('/'), endpoint.account.user_id, scope]).to_string()
}

/// Copies `from_item`'s attachments, with their content out of `files`, to
/// `to`'s item, then deletes its attachments `old`, as this run last wrote
/// it. The copies are uploaded and read back before `old` is deleted, so a
/// failed upload leaves the files it was replacing. Returns the item as the
/// server then holds it, and how many files it copied.
async fn copy_attachments(
    (files, from_item): (&Snapshot<'_>, &Item),
    (to, to_item): (&Endpoint<'_>, &Item),
    old: &[String],
    mode: Mode,
) -> Result<(Item, usize)> {
    let mut copied = Vec::new();
    for attachment in &from_item.attachments {
        let data = files.attachment(&attachment.id).with_context(|| {
            format!(
                "reading attachment {} of item {} from the source backup",
                attachment.id, from_item.id
            )
        })?;
        let id = to
            .account
            .upload_attachment(&to.scope, to_item, &attachment.file_name, &data)
            .await
            .with_context(|| format!("uploading an attachment to item {}", to_item.id))?;
        copied.push((
            id,
            attachment.file_name.as_str(),
            <[u8; 32]>::from(Sha256::digest(&data)),
        ));
    }
    if !copied.is_empty() {
        check_uploads(to, to_item, &copied, mode).await?;
    }
    for attachment_id in old {
        to.account
            .delete_attachment(&to_item.id, attachment_id)
            .await?;
    }
    let stored = reread(to, to_item, mode).await?;
    // Any other file was added since this run read the item, and is left for
    // the next run, which backs it up first.
    let wanted: BTreeSet<&str> = copied.iter().map(|(id, ..)| id.as_str()).collect();
    if attachment_ids(&stored) != wanted {
        bail!(
            "item {} changed during the run. Run again to copy it",
            to_item.id
        );
    }
    Ok((stored, copied.len()))
}

/// Reads `item` back and fails unless it holds each `uploaded` file, by id,
/// name and SHA-256 of its content.
async fn check_uploads(
    to: &Endpoint<'_>,
    item: &Item,
    uploaded: &[(String, &str, [u8; 32])],
    mode: Mode,
) -> Result<()> {
    let stored = reread(to, item, mode).await?;
    for (id, name, digest) in uploaded {
        let found = stored.attachments.iter().find(|a| a.id == *id);
        let matches = match found {
            Some(attachment) if attachment.file_name == *name => {
                let data = to
                    .account
                    .download_attachment(&to.scope, &stored, attachment)
                    .await?;
                <[u8; 32]>::from(Sha256::digest(&data)) == *digest
            }
            _ => false,
        };
        if !matches {
            bail!(
                "the attachments of item {} do not match what was uploaded",
                item.id
            );
        }
    }
    Ok(())
}

/// `item` as the server now holds it. Fails if it was edited since this run
/// wrote it: the next write would overwrite an edit that no backup holds yet.
async fn reread(endpoint: &Endpoint<'_>, item: &Item, mode: Mode) -> Result<Item> {
    let mut stored = endpoint
        .account
        .get_item(&endpoint.scope, &item.id)
        .await
        .with_context(|| format!("reading item {}", item.id))?;
    stored.collection_ids = item.collection_ids.clone();
    if comparable(&stored, None, &[], mode) != comparable(item, None, &[], mode)
        || stored.folder_id != item.folder_id
    {
        bail!(
            "item {} changed during the run. Run again to copy it",
            item.id
        );
    }
    Ok(stored)
}

fn ensure_distinct(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    source_vault: &Vault,
    dest_vault: &Vault,
) -> Result<()> {
    if vault_key(source) == vault_key(dest) {
        bail!("the source and destination are the same vault");
    }
    // Two members of one organization reach it through different accounts.
    if let (Scope::Organization(source), Scope::Organization(dest)) = (&source.scope, &dest.scope)
        && source == dest
    {
        bail!("the source and destination are the same organization");
    }
    // Item ids are unique, so shared ones mean one vault reached through two
    // server addresses.
    let dest_ids: HashSet<&str> = dest_vault.items.iter().map(|i| i.id.as_str()).collect();
    if (source_vault.items.iter()).any(|item| dest_ids.contains(item.id.as_str())) {
        bail!(
            "the source and destination hold the same items, so they are the same vault, \
             perhaps reached through two server addresses"
        );
    }
    Ok(())
}

fn mode(source: &Scope, dest: &Scope) -> Result<Mode> {
    match (source, dest) {
        (Scope::Personal, Scope::Personal) => Ok(Mode::Personal),
        (Scope::Organization(_), Scope::Organization(_)) => Ok(Mode::Organization),
        _ => bail!(
            "the source and destination must both be individual vaults or both be organizations"
        ),
    }
}

fn attachment_ids(item: &Item) -> BTreeSet<&str> {
    item.attachments.iter().map(|a| a.id.as_str()).collect()
}

/// Whether two reads of an item found it in the same state, in everything
/// the server reports.
fn same_state(left: &Item, right: &Item) -> bool {
    let canonical = |item: &Item| {
        let mut item = item.clone();
        item.collection_ids.sort_unstable();
        item.attachments.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        item
    };
    canonical(left) == canonical(right)
}

/// Fails if the plan deletes a copy of an item type the source server does
/// not show. Its source item is missing from the source vault, but may still
/// exist.
fn check_source_shows(plan: &Plan, dest: &Vault, traits: &ServerTraits) -> Result<()> {
    let hidden: Vec<&str> = (plan.deletes.iter())
        .filter(|delete| delete.reason == DeleteReason::Stale)
        .map(|delete| &dest.items[delete.dest])
        .filter(|item| traits.hides(item))
        .map(|item| item.id.as_str())
        .collect();
    if hidden.is_empty() {
        return Ok(());
    }
    bail!(
        "the source server does not show bank accounts, driver's licences or passports yet, so \
         the source items of {} of them may still exist ({}). If they were deleted, delete \
         those copies in the destination yourself",
        count(hidden.len(), "copy"),
        listed(&hidden)
    );
}

/// Fails if the destination cannot hold everything the run would copy. A
/// run that left something out would look complete, and the next run would
/// treat what is missing as deleted.
fn check_dest_holds(
    source: &Vault,
    plan: &Plan,
    traits: &ServerTraits,
    premium: bool,
    mode: Mode,
) -> Result<()> {
    let live = || source.items.iter().filter(|item| !item.in_trash);
    let hidden = live().filter(|item| traits.hides(item)).count();
    if hidden > 0 {
        bail!(
            "the destination server does not show bank accounts, driver's licences or passports \
             yet, so it cannot hold {} of the source. Leave them out with a filter, or use a \
             destination that shows them",
            count(hidden, "item")
        );
    }
    if mode == Mode::Organization && traits.items_need_collections {
        let homeless = live().filter(|item| item.collection_ids.is_empty()).count();
        if homeless > 0 {
            bail!(
                "{} of the source {} in no collection, which the destination server requires. \
                 Put {them} in a collection",
                count(homeless, "item"),
                if homeless == 1 { "is" } else { "are" },
                them = if homeless == 1 { "it" } else { "them" },
            );
        }
    }
    // Bitwarden can replace private member metadata that its API does not
    // expose for backup. No safety override can recover those values.
    if mode == Mode::Organization && !traits.organization_updates_safe && !plan.updates.is_empty() {
        bail!(
            "the destination server cannot safely update organization copies without removing \
             members' private folder, favorite or archive data. Use a Vaultwarden organization \
             as the destination for ongoing mirroring"
        );
    }
    // Organizations pay for attachments themselves.
    let files = plan.summary(source).attachments_copied;
    if mode == Mode::Personal && !premium && files > 0 {
        bail!(
            "the destination account has no premium subscription, which Bitwarden requires for \
             attachments, so the run cannot copy {}",
            count(files, "attachment")
        );
    }
    Ok(())
}

struct Run<'a> {
    source: &'a Endpoint<'a>,
    dest: &'a Endpoint<'a>,
    options: &'a MirrorOptions,
    folders: &'a FolderReceipts,
    reporter: &'a dyn Reporter,
    stage: &'static str,
    summary: Option<Summary>,
    warnings: Vec<String>,
}

impl<'a> Run<'a> {
    fn new(
        source: &'a Endpoint<'a>,
        dest: &'a Endpoint<'a>,
        options: &'a MirrorOptions,
        folders: &'a FolderReceipts,
        reporter: &'a dyn Reporter,
    ) -> Self {
        Self {
            source,
            dest,
            options,
            folders,
            reporter,
            stage: "planning",
            summary: None,
            warnings: Vec::new(),
        }
    }

    fn finish(self, result: Result<bool>) -> Result<Outcome, MirrorError> {
        match result {
            Ok(changed) => Ok(Outcome {
                summary: self.summary.unwrap_or_default(),
                changed,
                warnings: self.warnings,
            }),
            Err(error) => Err(MirrorError {
                stage: self.stage,
                summary: self.summary,
                source: error,
            }),
        }
    }

    /// Reduces the source to what the filter selects. A name that selects
    /// nothing is an error: a misspelt one would otherwise change what is
    /// mirrored, and could have every copy outside it deleted.
    fn filter(&mut self, source: &mut Vault, mode: Mode) -> Result<()> {
        let options = self.options;
        let filter = &options.filter;
        if filter.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = match mode {
            Mode::Personal => source.folders.iter().map(|f| f.name.as_str()).collect(),
            Mode::Organization => source.collections.iter().map(|c| c.name.as_str()).collect(),
        };
        let (kind, settings) = match mode {
            Mode::Personal => ("folder", ["INCLUDE_FOLDERS", "EXCLUDE_FOLDERS"]),
            Mode::Organization => ("collection", ["INCLUDE_COLLECTIONS", "EXCLUDE_COLLECTIONS"]),
        };
        // The names are vault content, so the error names only the setting.
        for (setting, patterns) in settings.into_iter().zip([&filter.include, &filter.exclude]) {
            let unmatched = (patterns.iter())
                .filter(|pattern| !names.iter().any(|name| Filter::matches(pattern, name)))
                .count();
            if unmatched > 0 {
                bail!(
                    "{} in {setting} {} no {kind} in the source. Check the spelling",
                    count(unmatched, "name"),
                    if unmatched == 1 { "matches" } else { "match" },
                );
            }
        }
        let before = source.items.iter().filter(|item| !item.in_trash).count();
        filter.apply(source, mode);
        let after = source.items.iter().filter(|item| !item.in_trash).count();
        self.reporter.status(
            "Filtered",
            &format!("{after} of {} by {kind}", count(before, "item")),
        );
        Ok(())
    }

    fn warn(&mut self, message: String) {
        self.reporter.warn(&message);
        self.warnings.push(message);
    }

    /// Plans and, given the source snapshot, applies and verifies. Returns
    /// whether the destination changed.
    async fn execute(
        &mut self,
        source_read: &VaultRead,
        dest_vault: &Vault,
        source_files: Option<&Snapshot<'_>>,
    ) -> Result<bool> {
        let mode = mode(&self.source.scope, &self.dest.scope)?;
        let source_vault = source_read.vault();
        ensure_distinct(self.source, self.dest, source_vault, dest_vault)?;
        let mut source_vault = source_vault.clone();
        self.filter(&mut source_vault, mode)?;
        let traits = self
            .dest
            .account
            .server_traits()
            .await
            .context("reading the destination server's settings")?;
        if self.options.skip_attachments {
            self.warn(
                "SKIP_ATTACHMENTS is on. Retained copies keep their attachments and new copies \
                 get none. Backups still include every attachment. Set \
                 SKIP_ATTACHMENTS=false to copy them again."
                    .to_string(),
            );
        }
        let pair = plan::pair(&vault_key(self.source), &vault_key(self.dest));
        let plan = plan::plan(
            &source_vault,
            dest_vault,
            mode,
            &pair,
            self.options.skip_attachments,
        );
        check_source_shows(&plan, dest_vault, source_read.traits())?;
        check_dest_holds(
            &source_vault,
            &plan,
            &traits,
            self.dest.account.premium,
            mode,
        )?;
        // Only an individual vault's folders are mirrored, and its own.
        let receipts = (mode == Mode::Personal)
            .then(|| self.folders.of(&plan::owner(&pair, &vault_key(self.dest))));
        let mut cleanup = match &receipts {
            Some(receipts) => {
                self.plan_cleanup(receipts, &plan, &source_vault, dest_vault)
                    .await
            }
            None => FolderCleanup::default(),
        };
        let mut summary = plan.summary(&source_vault);
        summary.folders_deleted = cleanup.obsolete.len();
        let (verb, plan_text) = describe_plan(&summary, source_files.is_none());
        self.reporter.status(verb, &plan_text);
        // A run reports the folders it deleted, not the ones it planned to.
        if source_files.is_some() {
            summary.folders_deleted = 0;
        }
        self.summary = Some(summary);
        if plan.reversed > 0 && plan.untracked > 0 {
            self.warn(format!(
                "The destination has {} that no source item was copied from. Runs leave them as \
                 they are, so if you deleted them in the source, delete them here too.",
                count(plan.untracked, "item")
            ));
        } else if plan.tracked_before == 0 && plan.untracked > 0 {
            self.warn(format!(
                "The destination has {} that vault-mirror didn't copy from this source. Runs \
                 leave them as they are.",
                count(plan.untracked, "item")
            ));
        }
        if plan.detached > 0 {
            self.warn(format!(
                "The destination has {} with the vault-mirror field of another copy, such as a \
                 cloned copy. Runs leave {them} alone. To keep {them} for good, remove the \
                 vault-mirror field.",
                count(plan.detached, "item"),
                them = if plan.detached == 1 { "it" } else { "them" },
            ));
        }
        plan::check_safety(&plan, dest_vault, &self.options.safety)?;
        let overwritten = plan.overwritten().count();
        if overwritten > 0 {
            self.warn(format!(
                "Replacing {} changed in the destination with the source version. The \
                 destination backup from the start of this run keeps the changes.",
                count(overwritten, "copy")
            ));
        }
        let Some(source_files) = source_files else {
            return Ok(false);
        };
        if let Some(receipts) = &receipts
            && let Err(error) = cleanup.stale.iter().try_for_each(|r| receipts.forget(r))
        {
            self.unrecorded(&error);
            cleanup.obsolete.clear();
        }
        let wrote = if plan.has_changes() {
            self.apply(
                &plan,
                &source_vault,
                dest_vault,
                source_files,
                receipts.as_ref(),
                mode,
            )
            .await?;
            self.stage = "verifying the destination";
            self.verify(&source_vault, &pair, mode).await?;
            self.reporter.status("Verified", "the destination");
            true
        } else if cleanup.obsolete.is_empty() {
            return Ok(false);
        } else {
            // Only folders are left to delete, which can wait for the next
            // run, so a failed read fails nothing.
            if let Err(error) = self.verify(&source_vault, &pair, mode).await {
                self.keep_folders(&cleanup.obsolete, &format!("{error:#}"));
                return Ok(false);
            }
            false
        };
        let deleted = match &receipts {
            Some(receipts) if !cleanup.obsolete.is_empty() => {
                self.delete_folders(&cleanup.obsolete, &source_vault, receipts)
                    .await
            }
            _ => 0,
        };
        Ok(wrote || deleted > 0)
    }

    /// The folders runs made that this run would delete, from the read it
    /// plans with. Any doubt keeps them, with a warning.
    async fn plan_cleanup(
        &mut self,
        receipts: &Receipts,
        plan: &Plan,
        source_vault: &Vault,
        dest_vault: &Vault,
    ) -> FolderCleanup {
        let made = match receipts.list() {
            Ok(made) => made,
            Err(error) => {
                self.warn(format!(
                    "Could not read which folders vault-mirror made, so runs keep them: {error:#}"
                ));
                return FolderCleanup::default();
            }
        };
        let mut cleanup = plan::folder_cleanup(&made, source_vault, dest_vault, Some(plan));
        if !cleanup.obsolete.is_empty()
            && let Err(error) = self.dest.account.load_folder_contents().await
        {
            self.keep_folders(&cleanup.obsolete, &format!("{error:#}"));
            cleanup.obsolete.clear();
        }
        cleanup
    }

    fn unrecorded(&mut self, error: &anyhow::Error) {
        self.warn(format!(
            "Could not update the record of the folders vault-mirror made, so this run deletes \
             no more of them: {error:#}"
        ));
    }

    fn keep_folders(&mut self, obsolete: &[Receipt], reason: &str) {
        if obsolete.is_empty() {
            return;
        }
        self.warn(format!(
            "Kept {} vault-mirror made that nothing is mirrored into: {reason}",
            count(obsolete.len(), "folder")
        ));
    }

    /// Deletes the `obsolete` folders that are still as they were planned,
    /// one by one, each after reading the destination again. Never fails:
    /// a folder kept is the safe outcome. Returns how many it deleted.
    async fn delete_folders(
        &mut self,
        obsolete: &[Receipt],
        source_vault: &Vault,
        receipts: &Receipts,
    ) -> usize {
        let account = self.dest.account;
        let mut deleted = 0;
        for (index, receipt) in obsolete.iter().enumerate() {
            let left = &obsolete[index..];
            let now = match account.load_folder_contents().await {
                Ok(now) => now,
                Err(error) => {
                    self.keep_folders(left, &format!("{error:#}"));
                    break;
                }
            };
            let made = BTreeSet::from([receipt.clone()]);
            let cleanup = plan::folder_cleanup(&made, source_vault, &now, None);
            if cleanup.stale.is_empty() {
                if cleanup.obsolete.is_empty() {
                    // Something was put in it, or its name selected, meanwhile.
                    continue;
                }
                match account.delete_folder(&receipt.folder_id).await {
                    Ok(gone) => {
                        deleted += usize::from(gone);
                    }
                    Err(error) => {
                        // The request may have arrived, so the receipt stays
                        // until a run sees the folder gone.
                        self.warn(format!(
                            "Could not confirm deletion of folder {}. The request may have \
                             reached the server. The next run rechecks before deleting any \
                             remaining folders: {error:#}",
                            receipt.folder_id
                        ));
                        break;
                    }
                }
            }
            if let Err(error) = receipts.forget(receipt) {
                self.unrecorded(&error);
                break;
            }
        }
        if deleted > 0 {
            self.reporter.status("Deleted", &count(deleted, "folder"));
        }
        if let Some(summary) = &mut self.summary {
            summary.folders_deleted = deleted;
        }
        deleted
    }

    async fn apply(
        &mut self,
        plan: &Plan,
        source_vault: &Vault,
        dest_vault: &Vault,
        source_files: &Snapshot<'_>,
        receipts: Option<&Receipts>,
        mode: Mode,
    ) -> Result<()> {
        let dest = self.dest;

        self.stage = "creating folders and collections";
        let mut folder_ids = plan.folder_ids.clone();
        let mut unrecorded = Vec::new();
        for name in &plan.folders_to_create {
            let folder = dest.account.create_folder(name).await?;
            // Recorded before anything else is written, so that a run that
            // stops later still knows the folder is its own.
            if let Some(receipts) = receipts {
                let recorded = (folder.stamp.clone())
                    .context("the server did not return its name")
                    .and_then(|stamp| {
                        receipts.record(&Receipt {
                            folder_id: folder.id.clone(),
                            stamp,
                        })
                    });
                if let Err(error) = recorded {
                    unrecorded.push(format!("folder {}: {error:#}", folder.id));
                }
            }
            folder_ids.insert(folder.name, folder.id);
        }
        if !unrecorded.is_empty() {
            self.warn(format!(
                "Could not record {} vault-mirror made, so runs will keep {}: {}",
                count(unrecorded.len(), "folder"),
                if unrecorded.len() == 1 { "it" } else { "them" },
                unrecorded.join("; ")
            ));
        }
        let mut collection_ids = plan.collection_ids.clone();
        if let Scope::Organization(org) = &dest.scope {
            for new in &plan.collections_to_create {
                let collection = dest
                    .account
                    .create_collection(org, &new.name, new.external_id.as_deref())
                    .await?;
                collection_ids.insert(collection.name, collection.id);
            }
        }
        // In an organization a folder or favorite is each member's own and
        // not mirrored. A new copy has neither, and a replacement keeps the
        // destination member's.
        let draft = |entry_index: usize, archived: bool, files: FileWrite<'_>| {
            let mut draft = plan.entries[entry_index].draft(
                source_vault,
                &folder_ids,
                &collection_ids,
                archived,
                files,
            )?;
            if mode == Mode::Organization {
                draft.content.insert("favorite".into(), Node::Bool(false));
            }
            anyhow::Ok(draft)
        };
        let replacement = |entry_index: usize, item: &Item, files: FileWrite<'_>| {
            let mut draft = draft(entry_index, item.archived, files)?;
            if mode == Mode::Organization {
                draft.folder_id = item.folder_id.clone();
                if let Some(favorite) = item.content.get("favorite") {
                    draft.content.insert("favorite".into(), favorite.clone());
                }
            }
            anyhow::Ok(draft)
        };
        let copies_files = |entry_index: usize| plan.entries[entry_index].copies_files();

        self.stage = "writing items";
        // Each write yields the entry it was for and the destination item as
        // the server now holds it. New items carry their marker from the
        // start, so a run that stops after creating one does not create it
        // again. An item whose attachments are still to be copied or removed
        // gets a marker that vouches for no attachments first, and its final
        // one once they are done and checked.
        let drafts = (plan.creates.iter())
            .map(|&entry_index| {
                let files = match copies_files(entry_index) {
                    true => FileWrite::Pending,
                    false => FileWrite::Kept,
                };
                draft(entry_index, false, files)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let folder_names: HashMap<String, String> = folder_ids
            .iter()
            .map(|(name, id)| (id.clone(), name.clone()))
            .collect();
        let rewritten = plan
            .updates
            .iter()
            .filter(|update| update.write_content || update.sync_attachments)
            .count();
        let writes = Tally::new(self.reporter, "Writing", drafts.len() + rewritten, "items");
        let new_items = dest
            .account
            .create_items(&dest.scope, &drafts, &folder_names, &|n| writes.add(n))
            .await?;
        let mut written: Vec<(usize, Item)> = plan.creates.iter().copied().zip(new_items).collect();
        let created = written.len();

        let updated: Vec<(usize, Item)> = stream::iter(plan.updates.iter().map(|update| {
            let (replacement, writes) = (&replacement, &writes);
            async move {
                let existing = &dest_vault.items[update.dest];
                let draft = if update.sync_attachments {
                    replacement(update.entry, existing, FileWrite::Pending)?
                } else if update.write_content {
                    replacement(update.entry, existing, FileWrite::Kept)?
                } else {
                    return anyhow::Ok((update.entry, existing.clone()));
                };
                let item = dest
                    .account
                    .update_item(&dest.scope, existing, &draft)
                    .await
                    .with_context(|| format!("updating destination item {}", existing.id))?;
                writes.add(1);
                Ok((update.entry, item))
            }
        }))
        .buffer_unordered(WRITES_AT_ONCE)
        .try_collect()
        .await?;
        written.extend(updated);
        if created > 0 {
            self.reporter.status("Created", &count(created, "item"));
        }
        if rewritten > 0 {
            self.reporter.status("Updated", &count(rewritten, "item"));
        }

        self.stage = "copying attachments";
        let old_files: HashMap<usize, Vec<String>> = (plan.updates.iter())
            .filter(|update| update.sync_attachments)
            .map(|update| {
                let old = dest_vault.items[update.dest].attachments.iter();
                (update.entry, old.map(|a| a.id.clone()).collect())
            })
            .collect();
        let transfers: Vec<(usize, &Item, &[String])> = (written[..created].iter())
            .filter(|(entry_index, _)| copies_files(*entry_index))
            .map(|(entry_index, item)| (*entry_index, item, &[][..]))
            .chain(
                (written[created..].iter()).filter_map(|(entry_index, item)| {
                    let old = old_files.get(entry_index)?;
                    Some((*entry_index, item, old.as_slice()))
                }),
            )
            .collect();
        let uploads = Tally::new(
            self.reporter,
            "Copying",
            (transfers.iter())
                .map(|(entry_index, ..)| {
                    source_vault.items[plan.entries[*entry_index].source]
                        .attachments
                        .len()
                })
                .sum(),
            "attachments",
        );
        let copied: Vec<(usize, usize, Item)> =
            stream::iter(transfers.into_iter().map(|(entry_index, dest_item, old)| {
                let (replacement, uploads) = (&replacement, &uploads);
                async move {
                    let source_item = &source_vault.items[plan.entries[entry_index].source];
                    let (stored, copied) =
                        copy_attachments((source_files, source_item), (dest, dest_item), old, mode)
                            .await?;
                    uploads.add(copied);
                    let draft =
                        replacement(entry_index, &stored, FileWrite::Copied(&stored.attachments))?;
                    let finished = dest
                        .account
                        .update_item(&dest.scope, &stored, &draft)
                        .await
                        .with_context(|| format!("updating destination item {}", dest_item.id))?;
                    anyhow::Ok((entry_index, copied, finished))
                }
            }))
            // Attachments are held in memory while they are re-encrypted.
            .buffer_unordered(2)
            .try_collect()
            .await?;
        let mut total = 0;
        let mut finished: HashMap<usize, Item> = HashMap::new();
        for (entry_index, copied, item) in copied {
            total += copied;
            finished.insert(entry_index, item);
        }
        for (entry_index, item) in &mut written {
            if let Some(done) = finished.remove(entry_index) {
                *item = done;
            }
        }
        if total > 0 {
            self.reporter.status("Copied", &count(total, "attachment"));
        }

        self.stage = "setting archive state";
        // Decided from what the server reports after the writes, not from the
        // plan: servers differ in what a write, or an attachment upload, does
        // to an item's archive state.
        let wanted =
            |entry_index: usize| source_vault.items[plan.entries[entry_index].source].archived;
        let changing: Vec<&(usize, Item)> = (written.iter())
            .filter(|(entry_index, item)| item.archived != wanted(*entry_index))
            .collect();
        for archived in [true, false] {
            let ids: Vec<String> = (changing.iter())
                .filter(|(entry_index, _)| wanted(*entry_index) == archived)
                .map(|(_, item)| item.id.clone())
                .collect();
            if ids.is_empty() {
                continue;
            }
            // Archiving is a paid feature on some servers and missing on
            // older ones. A copy left in the wrong state is no complete copy.
            dest.account
                .set_archived(&ids, archived)
                .await
                .with_context(|| {
                    format!(
                        "the destination refused to {} {}",
                        if archived { "archive" } else { "unarchive" },
                        count(ids.len(), "item")
                    )
                })?;
        }
        // Each copy's marker records the archive state it was written in,
        // so a run that stops before the state is set takes no copy for
        // changed by hand. Now the state is set, the marker follows.
        stream::iter(changing.into_iter().map(|(entry_index, item)| {
            let (replacement, copies_files) = (&replacement, &copies_files);
            async move {
                let stored = reread(dest, item, mode).await?;
                let files = match copies_files(*entry_index) {
                    true => FileWrite::Copied(&stored.attachments),
                    false => FileWrite::Kept,
                };
                let draft = replacement(*entry_index, &stored, files)?;
                (dest.account.update_item(&dest.scope, &stored, &draft).await)
                    .with_context(|| format!("updating destination item {}", item.id))?;
                anyhow::Ok(())
            }
        }))
        .buffer_unordered(WRITES_AT_ONCE)
        .try_collect::<Vec<()>>()
        .await?;

        // Deleting items comes last, so a run that fails before has removed
        // no item.
        self.stage = "deleting obsolete items";
        if !plan.deletes.is_empty() {
            // The plan comes from the backup at the start of the run. An item
            // that changed in any way since holds what no backup does. The
            // request names items by id only, so an edit between this read
            // and it can still be lost; the trash keeps a way back.
            let now = dest.account.load(&dest.scope).await?;
            let now: HashMap<&str, &Item> = (now.items.iter())
                .map(|item| (item.id.as_str(), item))
                .collect();
            let mut ids = Vec::new();
            for delete in &plan.deletes {
                let planned = &dest_vault.items[delete.dest];
                let Some(item) = now.get(planned.id.as_str()) else {
                    continue;
                };
                if !same_state(item, planned) {
                    bail!(
                        "destination item {} changed during the run. Run again to mirror it",
                        planned.id
                    );
                }
                ids.push(planned.id.clone());
            }
            if !ids.is_empty() {
                dest.account.trash_items(&ids).await?;
                self.reporter.status("Deleted", &count(ids.len(), "item"));
            }
            // Items already gone were not deleted by this run.
            if let Some(summary) = &mut self.summary {
                summary.items_deleted = ids.len();
            }
        }
        Ok(())
    }

    /// Reads the destination again and checks that nothing is left to do.
    /// Attachments were checked as they were copied.
    async fn verify(&mut self, source_vault: &Vault, pair: &str, mode: Mode) -> Result<()> {
        let dest = self.dest;
        let dest_vault = dest.account.load(&dest.scope).await?;
        let again = plan::plan(
            source_vault,
            &dest_vault,
            mode,
            pair,
            self.options.skip_attachments,
        );
        if !again.has_changes() {
            return Ok(());
        }
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

/// The plan's changes in a few words, leaving out what does not happen, as
/// a verb and the rest of the sentence.
fn describe_plan(summary: &Summary, check: bool) -> (&'static str, String) {
    let changes: Vec<String> = [
        (summary.items_created, "create", "item"),
        (summary.items_updated, "update", "item"),
        (summary.items_deleted, "delete", "item"),
        (summary.folders_created, "create", "folder"),
        (summary.folders_deleted, "delete", "folder"),
        (summary.collections_created, "create", "collection"),
        (summary.attachments_copied, "copy", "attachment"),
    ]
    .into_iter()
    .filter(|(count, ..)| *count > 0)
    .map(|(n, verb, noun)| format!("{verb} {}", count(n, noun)))
    .collect();
    let unchanged = format!("{} unchanged", count(summary.items_unchanged, "item"));
    let changes = changes.join(", ");
    match (changes.is_empty(), check) {
        (true, false) => ("Planned", format!("no changes; {unchanged}")),
        (true, true) => ("Would", format!("change nothing; {unchanged}")),
        (false, false) => ("Planned", format!("to {changes}; {unchanged}")),
        (false, true) => ("Would", format!("{changes}; {unchanged}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{Attachment, Collection, Folder};

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
            content: std::collections::BTreeMap::new(),
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
    fn two_reads_of_an_item_differ_in_anything_the_server_reports() {
        let mut read = item("i", Some("f"), &["c1", "c2"]);
        read.revision_date = Some("2026-10-16T12:00:00.000Z".into());
        let attachment = |id: &str| Attachment {
            id: id.into(),
            file_name: "scan.pdf".into(),
            size: 1,
            wrapped_key: None,
        };
        read.attachments = vec![attachment("a1"), attachment("a2")];
        let mut reordered = read.clone();
        reordered.collection_ids.reverse();
        reordered.attachments.reverse();
        assert!(same_state(&read, &reordered));

        let changes: [fn(&mut Item); 9] = [
            |item| item.revision_date = Some("2026-10-16T13:00:00.000Z".into()),
            |item| item.in_trash = true,
            |item| item.folder_id = None,
            |item| item.collection_ids.push("c3".into()),
            |item| item.archived_date = Some("2026-10-16T13:00:00.000Z".into()),
            |item| item.wrapped_key = Some("another key".into()),
            |item| item.attachments[0].size = 2,
            |item| item.attachments[1].wrapped_key = Some("another key".into()),
            |item| {
                item.content
                    .insert("notes".into(), crate::node::Node::secret("edited"));
            },
        ];
        for (index, change) in changes.iter().enumerate() {
            let mut changed = read.clone();
            change(&mut changed);
            assert!(!same_state(&read, &changed), "change {index}");
        }
    }

    #[test]
    fn copies_of_a_type_the_source_hides_are_not_taken_for_copies_of_deleted_items() {
        let mut copy = item("copy", None, &[]);
        copy.content
            .insert("type".into(), crate::node::Node::Number(8.into()));
        let dest = Vault {
            items: vec![copy],
            ..Vault::default()
        };
        let plan = |reason| Plan {
            deletes: vec![plan::Delete {
                dest: 0,
                reason,
                changed_in_dest: plan::Changes::default(),
            }],
            ..Plan::default()
        };
        let traits = |hidden_item_types| ServerTraits {
            hidden_item_types,
            items_need_collections: false,
            organization_updates_safe: true,
        };
        let refused = check_source_shows(&plan(DeleteReason::Stale), &dest, &traits(vec![6, 7, 8]));
        assert!(format!("{:#}", refused.unwrap_err()).contains("(copy)"));
        assert!(check_source_shows(&plan(DeleteReason::Stale), &dest, &traits(vec![])).is_ok());
        let duplicate = plan(DeleteReason::Duplicate);
        assert!(check_source_shows(&duplicate, &dest, &traits(vec![6, 7, 8])).is_ok());
    }

    mod visibility {
        use super::*;
        use std::{
            collections::VecDeque,
            io::{Read, Write},
            net::TcpListener,
            sync::{Arc, LazyLock, Mutex, atomic::AtomicBool},
            thread,
            time::Duration,
        };

        use crate::{
            api::tests::http_response,
            backup::{BackupOptions, backup},
            crypto::SymKey,
            snapshot,
            store::{Keys, Store},
        };
        use serde_json::{Value, json};
        use time::OffsetDateTime;

        #[derive(Default)]
        struct State {
            shown: VecDeque<bool>,
            source: Value,
            dest: Value,
            requests: Vec<String>,
        }

        struct Server {
            url: String,
            state: Arc<Mutex<State>>,
            stopped: Arc<AtomicBool>,
            task: Option<thread::JoinHandle<()>>,
        }

        impl Server {
            fn start(shown: &[bool]) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                listener.set_nonblocking(true).unwrap();
                let state = Arc::new(Mutex::new(State {
                    shown: shown.iter().copied().collect(),
                    ..State::default()
                }));
                let stopped = Arc::new(AtomicBool::new(false));
                let (received, done) = (state.clone(), stopped.clone());
                let task = thread::spawn(move || {
                    while !done.load(Ordering::SeqCst) {
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            Err(error) => panic!("{error}"),
                        };
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = Vec::new();
                        let mut buffer = [0; 4096];
                        let (head, body) = loop {
                            let size = stream.read(&mut buffer).unwrap();
                            assert_ne!(size, 0);
                            request.extend_from_slice(&buffer[..size]);
                            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&request[..end]).into_owned();
                                let length = head
                                    .lines()
                                    .find_map(|line| {
                                        let (name, value) = line.split_once(':')?;
                                        name.eq_ignore_ascii_case("content-length")
                                            .then(|| value.trim().parse::<usize>().unwrap())
                                    })
                                    .unwrap_or(0);
                                if request.len() >= end + 4 + length {
                                    break (head, request[end + 4..].to_vec());
                                }
                            }
                        };
                        let first = head.lines().next().unwrap();
                        let mut words = first.split_whitespace();
                        let method = words.next().unwrap();
                        let path = words.next().unwrap();
                        let mut state = received.lock().unwrap();
                        state.requests.push(format!("{method} {path}"));
                        let mut status = "200 OK";
                        let response = match (method, path) {
                            ("POST", path) if path.ends_with("/identity/connect/token") => {
                                json!({"access_token": "synthetic", "expires_in": 3600})
                            }
                            ("GET", "/source/api/config") => {
                                let shown = if state.shown.len() > 1 {
                                    state.shown.pop_front().unwrap()
                                } else {
                                    *state.shown.front().unwrap()
                                };
                                json!({"server": null, "featureStates": {
                                    "pm-32009-new-item-types": shown
                                }})
                            }
                            ("GET", "/dest/api/config") => {
                                json!({"server": {"name": "Vaultwarden"}})
                            }
                            ("GET", "/source/api/sync?excludeDomains=true") => state.source.clone(),
                            ("GET", "/dest/api/sync?excludeDomains=true") => state.dest.clone(),
                            ("PUT", "/dest/api/ciphers/delete") => {
                                let body: Value = serde_json::from_slice(&body).unwrap();
                                assert_eq!(body, json!({"ids": ["copy-passport"]}));
                                state.dest["ciphers"]
                                    .as_array_mut()
                                    .unwrap()
                                    .retain(|item| item["id"] != "copy-passport");
                                json!({})
                            }
                            _ => {
                                status = "400 Bad Request";
                                json!({"message": "unexpected request"})
                            }
                        };
                        let response = http_response(
                            status,
                            "Content-Type: application/json\r\n",
                            &response.to_string(),
                        );
                        stream.write_all(response.as_bytes()).unwrap();
                    }
                });
                Self {
                    url,
                    state,
                    stopped,
                    task: Some(task),
                }
            }

            fn requests(&self) -> Vec<String> {
                self.state
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .filter(|request| !request.contains("/identity/"))
                    .cloned()
                    .collect()
            }

            async fn accounts(&self) -> (Account, Account) {
                let source_key = SymKey::generate();
                let dest_key = SymKey::generate();
                let source =
                    crate::vault::tests::account(&format!("{}/source", self.url), source_key).await;
                let dest =
                    crate::vault::tests::account(&format!("{}/dest", self.url), dest_key).await;
                let pair = plan::pair(&vault_key(&endpoint(&source)), &vault_key(&endpoint(&dest)));
                let mut original = Vault::default();
                for (id, kind) in [("passport", 8), ("kept-1", 2), ("kept-2", 2)] {
                    let mut item = item(id, None, &[]);
                    item.content = [
                        ("type".into(), Node::Number(kind.into())),
                        ("name".into(), Node::secret("synthetic vault content")),
                    ]
                    .into();
                    original.items.push(item);
                }
                let plan = plan::plan(&original, &Vault::default(), Mode::Personal, &pair, false);
                let mut copies = Vec::new();
                for entry in &plan.entries {
                    let draft = entry
                        .draft(
                            &original,
                            &HashMap::new(),
                            &HashMap::new(),
                            false,
                            FileWrite::Kept,
                        )
                        .unwrap();
                    let mut copy = original.items[entry.source].clone();
                    copy.id = format!("copy-{}", copy.id);
                    copy.content = draft.content;
                    copies.push(copy);
                }
                let mut state = self.state.lock().unwrap();
                state.source = source.test_sync(&original.items[1..]);
                state.dest = dest.test_sync(&copies);
                (source, dest)
            }
        }

        impl Drop for Server {
            fn drop(&mut self) {
                self.stopped.store(true, Ordering::SeqCst);
                self.task.take().unwrap().join().unwrap();
            }
        }

        struct Quiet;

        impl Reporter for Quiet {
            fn status(&self, _: &str, _: &str) {}
            fn warn(&self, _: &str) {}
        }

        static BACKUPS: LazyLock<(tempfile::TempDir, Keys)> = LazyLock::new(|| {
            let dir = tempfile::tempdir().unwrap();
            let keys = Keys::open(dir.path(), "synthetic passphrase").unwrap();
            (dir, keys)
        });

        fn backup_options() -> BackupOptions {
            BackupOptions {
                retention_days: 1,
                now: OffsetDateTime::UNIX_EPOCH + time::Duration::days(10),
            }
        }

        fn endpoint(account: &Account) -> Endpoint<'_> {
            Endpoint {
                account,
                scope: Scope::Personal,
            }
        }

        async fn evaluate(
            source: &Account,
            dest: &Account,
            source_read: &Protected<'_>,
            store: &Store<'_>,
            run: bool,
        ) -> Result<Outcome> {
            let options = MirrorOptions::default();
            let folders = FolderReceipts::new(BACKUPS.0.path());
            Ok(if run {
                let (_, dest_read) =
                    backup(dest, &Scope::Personal, store, &backup_options(), &Quiet).await?;
                mirror(
                    &endpoint(source),
                    &endpoint(dest),
                    source_read,
                    &dest_read,
                    &options,
                    &folders,
                    &Quiet,
                )
                .await?
            } else {
                let dest_vault = dest.load(&Scope::Personal).await?;
                check(
                    &endpoint(source),
                    &endpoint(dest),
                    &source_read.read,
                    &dest_vault,
                    &options,
                    &folders,
                    &Quiet,
                )
                .await?
            })
        }

        fn source_order() -> Vec<String> {
            [
                "GET /source/api/config",
                "GET /source/api/sync?excludeDomains=true",
                "GET /source/api/config",
            ]
            .map(str::to_string)
            .into()
        }

        #[tokio::test]
        async fn observed_visibility_transitions_refuse_before_snapshot_commit_or_prune() {
            for shown in [[false, true], [true, false]] {
                let server = Server::start(&shown);
                let (source, dest) = server.accounts().await;
                let (dir, keys) = &*BACKUPS;
                let store = Store::new(dir.path(), &uuid::Uuid::new_v4().to_string(), keys);
                let document = snapshot::build(&Vault::default(), &Scope::Personal).unwrap();
                let oldest = store
                    .begin(OffsetDateTime::UNIX_EPOCH)
                    .unwrap()
                    .finish(&document)
                    .unwrap();
                let newest = store
                    .begin(OffsetDateTime::UNIX_EPOCH + time::Duration::days(1))
                    .unwrap()
                    .finish(&document)
                    .unwrap();
                let result = async {
                    let (_, read) =
                        backup(&source, &Scope::Personal, &store, &backup_options(), &Quiet)
                            .await?;
                    evaluate(&source, &dest, &read, &store, true).await
                }
                .await;
                let error = match result {
                    Ok(_) => panic!("a visibility transition was accepted"),
                    Err(error) => error,
                };
                assert!(
                    format!("{error:#}").contains("visibility changed"),
                    "{error:#}"
                );
                assert_eq!(server.requests(), source_order());
                assert_eq!(store.list().unwrap(), [oldest, newest]);
            }
        }

        #[tokio::test]
        async fn observed_visibility_transitions_also_refuse_checks() {
            for shown in [[false, true], [true, false]] {
                let server = Server::start(&shown);
                let (source, dest) = server.accounts().await;
                let result = async {
                    let source_read = source.load_with_traits(&Scope::Personal).await?;
                    let dest_vault = dest.load(&Scope::Personal).await?;
                    check(
                        &endpoint(&source),
                        &endpoint(&dest),
                        &source_read,
                        &dest_vault,
                        &MirrorOptions::default(),
                        &FolderReceipts::new(BACKUPS.0.path()),
                        &Quiet,
                    )
                    .await?;
                    anyhow::Ok(())
                }
                .await;
                let error = result.unwrap_err();
                assert!(
                    format!("{error:#}").contains("visibility changed"),
                    "{error:#}"
                );
                assert_eq!(server.requests(), source_order());
            }
        }

        #[tokio::test]
        async fn hidden_read_stays_restrictive_after_later_enablement_for_run_and_check() {
            for run in [false, true] {
                let server = Server::start(&[false, false, true]);
                let (source, dest) = server.accounts().await;
                let (dir, keys) = &*BACKUPS;
                let store = Store::new(dir.path(), &uuid::Uuid::new_v4().to_string(), keys);
                let (_, source_read) =
                    backup(&source, &Scope::Personal, &store, &backup_options(), &Quiet)
                        .await
                        .unwrap();
                assert!(
                    source
                        .server_traits()
                        .await
                        .unwrap()
                        .hidden_item_types
                        .is_empty()
                );
                let result = evaluate(&source, &dest, &source_read, &store, run).await;
                let error = result.unwrap_err();
                assert!(
                    format!("{error:#}").contains("source server does not show"),
                    "{error:#}"
                );
                assert!(!format!("{error:#}").contains("synthetic vault content"));
                let mut expected = source_order();
                expected.push("GET /source/api/config".into());
                if run {
                    expected.push("GET /dest/api/config".into());
                }
                expected.push("GET /dest/api/sync?excludeDomains=true".into());
                if run {
                    expected.push("GET /dest/api/config".into());
                }
                expected.push("GET /dest/api/config".into());
                assert_eq!(server.requests(), expected);
            }
        }

        #[tokio::test]
        async fn enabled_read_allows_a_genuine_deletion_for_run_and_check() {
            for run in [false, true] {
                let server = Server::start(&[true, true]);
                let (source, dest) = server.accounts().await;
                let (dir, keys) = &*BACKUPS;
                let store = Store::new(dir.path(), &uuid::Uuid::new_v4().to_string(), keys);
                let (_, source_read) =
                    backup(&source, &Scope::Personal, &store, &backup_options(), &Quiet)
                        .await
                        .unwrap();
                let result = evaluate(&source, &dest, &source_read, &store, run).await;
                let outcome = result.unwrap();
                assert_eq!(outcome.summary.items_deleted, 1);
                assert_eq!(outcome.summary.items_unchanged, 2);
                assert_eq!(outcome.changed, run);
                let mut expected = source_order();
                if run {
                    expected.push("GET /dest/api/config".into());
                }
                expected.push("GET /dest/api/sync?excludeDomains=true".into());
                if run {
                    expected.push("GET /dest/api/config".into());
                }
                expected.push("GET /dest/api/config".into());
                if run {
                    expected.extend(
                        [
                            "GET /dest/api/sync?excludeDomains=true",
                            "PUT /dest/api/ciphers/delete",
                            "GET /dest/api/sync?excludeDomains=true",
                        ]
                        .map(str::to_string),
                    );
                }
                assert_eq!(server.requests(), expected);
            }
        }
    }

    #[test]
    fn plans_are_described_by_what_changes() {
        let summary = Summary {
            items_created: 9,
            items_unchanged: 1,
            folders_created: 1,
            folders_deleted: 2,
            attachments_copied: 4,
            ..Summary::default()
        };
        let changes = "create 9 items, create 1 folder, delete 2 folders, copy 4 attachments; 1 \
                       item unchanged";
        assert_eq!(
            describe_plan(&summary, false),
            ("Planned", format!("to {changes}"))
        );
        assert_eq!(
            describe_plan(&summary, true),
            ("Would", changes.to_string())
        );
        let nothing = Summary {
            items_unchanged: 9,
            ..Summary::default()
        };
        assert_eq!(
            describe_plan(&nothing, false),
            ("Planned", "no changes; 9 items unchanged".to_string())
        );
        assert_eq!(
            describe_plan(&nothing, true),
            ("Would", "change nothing; 9 items unchanged".to_string())
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
                    stamp: None,
                })
                .collect(),
            items: vec![
                item("work", Some("Work"), &[]),
                item("infra", Some("Work/Infra"), &[]),
                item("shop", Some("Workshop"), &[]),
                item("private", Some("Private"), &[]),
                item("unfiled", None, &[]),
            ],
            ..Vault::default()
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
            ..Vault::default()
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
