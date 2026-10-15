//! Makes a destination vault match a source vault.

use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use sha2::{Digest, Sha256};

use crate::{
    api::ApiError,
    plan::{self, Mode, Plan, Safety, Summary, comparable, differing_paths},
    text::count,
    vault::{Account, Draft, Item, Scope, Vault},
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
    /// A backup of the destination was saved before this mirror, so it holds
    /// whatever the mirror replaces.
    pub dest_backed_up: bool,
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
            dest_backed_up: false,
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

pub async fn mirror(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    options: &MirrorOptions,
    reporter: &dyn Reporter,
) -> Result<Outcome, MirrorError> {
    mirror_onto(source, dest, None, options, reporter).await
}

/// Like `mirror`, but plans against `dest_vault`, the destination as it was
/// just backed up, instead of reading it again. An item added in between is
/// then left alone until a later run has backed it up.
pub async fn mirror_onto(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    dest_vault: Option<Vault>,
    options: &MirrorOptions,
    reporter: &dyn Reporter,
) -> Result<Outcome, MirrorError> {
    let mut run = Run {
        dest_vault,
        source,
        dest,
        options,
        reporter,
        stage: "reading the vaults",
        summary: None,
        warnings: Vec::new(),
        archive_unsupported: false,
        copy_attachments: true,
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

/// What a pull copied into the source.
#[derive(Debug, Default)]
pub struct Pulled {
    pub summary: Summary,
    pub warnings: Vec<String>,
}

/// Copies into the source what changed in the destination since vault-mirror
/// wrote it, for when the destination was used while the source was down.
/// Edited copies replace their originals, and items vault-mirror didn't make
/// are added. Nothing is deleted. The vaults are the ones backed up, and every
/// write names the revision it replaces, so the server refuses to overwrite a
/// source edit made since. `backed_up` says whether a backup holds what this
/// replaces.
///
/// Each item it writes gets a marker naming the destination item, so that the
/// mirror after it pairs the two even when it is interrupted and run again.
/// `unmark` removes those markers once the mirror has run.
pub async fn pull(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    source_vault: &Vault,
    dest_vault: &Vault,
    options: &MirrorOptions,
    backed_up: bool,
    reporter: &dyn Reporter,
) -> Result<Pulled> {
    let mode = mode(&source.scope, &dest.scope)?;
    ensure_distinct(source, dest, source_vault, dest_vault)?;
    let plan = plan::plan(source_vault, dest_vault, mode);
    let mut pulled = Pulled::default();
    let folder_name = |id: Option<&str>| {
        let id = id?;
        (dest_vault.folders.iter())
            .find(|f| f.id == id)
            .map(|f| f.name.clone())
    };
    let collection_names = |item: &Item| -> Vec<String> {
        let mut names: Vec<String> = (dest_vault.collections.iter())
            .filter(|c| item.collection_ids.contains(&c.id))
            .map(|c| c.name.clone())
            .collect();
        names.sort_unstable();
        names
    };
    let mut source_folders: HashMap<String, String> = (source_vault.folders.iter())
        .map(|f| (f.name.clone(), f.id.clone()))
        .collect();
    let mut source_collections: HashMap<String, String> = (source_vault.collections.iter())
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();

    // Source items that name a destination item come from an earlier pull.
    let linked: HashMap<String, &Item> = (source_vault.items.iter())
        .filter(|item| !item.in_trash)
        .filter_map(|item| Some((plan::marker_of(item)?.source_id, item)))
        .collect();
    let mut selected = dest_vault.clone();
    options.filter.apply(&mut selected, mode);
    let selected: HashSet<&str> = selected.items.iter().map(|i| i.id.as_str()).collect();
    // Items vault-mirror didn't make, each with its source item if an earlier
    // pull made one but stopped before its attachments were done.
    let added: Vec<(&Item, Option<&Item>)> = (dest_vault.items.iter())
        .filter(|item| {
            !item.in_trash && plan::marker_of(item).is_none() && selected.contains(item.id.as_str())
        })
        .filter_map(|item| match linked.get(&item.id) {
            None => Some((item, None)),
            Some(&made) => (plan::marker_of(made)?.files_pending()).then_some((item, Some(made))),
        })
        .collect();
    // A resumed item is finished below, whatever changed in it.
    let resumed: HashSet<&str> = (added.iter())
        .filter(|(_, made)| made.is_some())
        .map(|(item, _)| item.id.as_str())
        .collect();
    let changed: Vec<&plan::Update> = (plan.updates.iter())
        .filter(|update| {
            update.changed_in_dest() && !resumed.contains(dest_vault.items[update.dest].id.as_str())
        })
        .collect();
    let both = changed
        .iter()
        .filter(|update| {
            let entry = &plan.entries[update.entry];
            plan::marker_of(&dest_vault.items[update.dest])
                .is_some_and(|marker| marker.content != entry.marker.content)
        })
        .count();
    if both > 0 {
        let kept = match backed_up {
            true => ", which is in the source backup from the start of this run",
            false => "",
        };
        let warning = format!(
            "{} changed in both vaults since the last run. The destination's version \
             replaces the source's{kept}.",
            count(both, "item")
        );
        reporter.warn(&warning);
        pulled.warnings.push(warning);
    }

    pulled.summary.items_updated = changed.len();
    pulled.summary.items_created = added.len();
    let items: Vec<String> = [(added.len(), "new item"), (changed.len(), "changed item")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, noun)| count(n, noun))
        .collect();
    match (items.is_empty(), options.dry_run) {
        (true, _) => reporter.status("Found", "nothing to pull from the destination"),
        (false, true) => reporter.status(
            "Would pull",
            &format!("{} into the source", items.join(" and ")),
        ),
        (false, false) => reporter.status(
            "Pulling",
            &format!("{} into the source", items.join(" and ")),
        ),
    }
    if options.dry_run {
        return Ok(pulled);
    }

    for update in changed {
        let original = &source_vault.items[plan.entries[update.entry].source];
        let copy = &dest_vault.items[update.dest];
        let folder = folder_name(copy.folder_id.as_deref());
        // An edit brings its folder along. A copy whose files alone changed
        // leaves the original where it is.
        let (content, folder_id) = match (update.content_changed_in_dest, mode) {
            (true, Mode::Personal) => (
                copy,
                source_folder(source, &mut source_folders, folder.as_deref()).await?,
            ),
            (true, Mode::Organization) => (copy, original.folder_id.clone()),
            (false, _) => (original, original.folder_id.clone()),
        };
        let names = collection_names(copy);
        let draft = |files: Option<&Item>| Draft {
            content: plan::mark_copy(content, copy, folder.as_deref(), &names, mode, files),
            folder_id: folder_id.clone(),
            collection_ids: original.collection_ids.clone(),
        };
        // The files match the copy's once copied, or if neither side changed them.
        let files = match update.files_changed_in_dest {
            true => None,
            false => (!update.sync_attachments).then_some(original),
        };
        let written = source
            .account
            .update_item(&source.scope, original, &draft(files))
            .await
            .with_context(|| format!("updating source item {}", original.id))?;
        if !update.files_changed_in_dest {
            continue;
        }
        let old: Vec<String> = original.attachments.iter().map(|a| a.id.clone()).collect();
        let (stored, copied) = copy_attachments(
            (dest, copy),
            (source, &written),
            &old,
            options.verify_attachments,
            mode,
        )
        .await?;
        pulled.summary.attachments_copied += copied;
        source
            .account
            .update_item(&source.scope, &stored, &draft(Some(&stored)))
            .await
            .with_context(|| format!("updating source item {}", stored.id))?;
    }

    let mut not_archived = 0;
    let mut archive_refusal = None;
    for (item, made) in added {
        let folder = folder_name(item.folder_id.as_deref());
        let names = collection_names(item);
        // A resumed item keeps what changed in the source since, unless the
        // destination item changed too.
        let kept = made.filter(|made| {
            plan::marker_of(made)
                .is_some_and(|marker| marker.content_matches(item, folder.as_deref(), &names, mode))
        });
        let (content, folder_id, collection_ids) = match kept {
            Some(made) => (made, made.folder_id.clone(), made.collection_ids.clone()),
            None => {
                let folder_id = match mode {
                    Mode::Personal => {
                        source_folder(source, &mut source_folders, folder.as_deref()).await?
                    }
                    Mode::Organization => None,
                };
                let mut collection_ids = Vec::new();
                if let Scope::Organization(org) = &source.scope {
                    for name in &names {
                        if !source_collections.contains_key(name) {
                            let created = source.account.create_collection(org, name, None).await?;
                            source_collections.insert(created.name, created.id);
                        }
                        collection_ids.push(source_collections[name].clone());
                    }
                }
                (item, folder_id, collection_ids)
            }
        };
        let draft = |files: Option<&Item>| Draft {
            content: plan::mark_copy(content, item, folder.as_deref(), &names, mode, files),
            folder_id: folder_id.clone(),
            collection_ids: collection_ids.clone(),
        };
        let made = match made {
            Some(made) => made.clone(),
            None => {
                source
                    .account
                    .create_item(&source.scope, &draft(None))
                    .await?
            }
        };
        let old: Vec<String> = made.attachments.iter().map(|a| a.id.clone()).collect();
        let (mut stored, copied) = copy_attachments(
            (dest, item),
            (source, &made),
            &old,
            options.verify_attachments,
            mode,
        )
        .await?;
        pulled.summary.attachments_copied += copied;
        // Archived before the final marker, so that a pull that stops in
        // between finishes the item, archive state included, when run again.
        if item.archived && !stored.archived {
            match archive_refusal {
                Some(_) => not_archived += 1,
                None => match source
                    .account
                    .set_archived(&[stored.id.clone()], true)
                    .await
                {
                    Ok(()) => {
                        let mut archived =
                            source.account.get_item(&source.scope, &stored.id).await?;
                        archived.collection_ids = stored.collection_ids.clone();
                        if comparable(&archived, None, &[], mode)
                            != comparable(&stored, None, &[], mode)
                            || archived.folder_id != stored.folder_id
                        {
                            bail!(
                                "item {} changed during the run. Run again to copy it",
                                stored.id
                            );
                        }
                        stored = archived;
                    }
                    Err(error) if refused(&error) => {
                        not_archived += 1;
                        archive_refusal = Some(error);
                    }
                    Err(error) => return Err(error),
                },
            }
        }
        source
            .account
            .update_item(&source.scope, &stored, &draft(Some(&stored)))
            .await
            .with_context(|| format!("updating source item {}", stored.id))?;
    }
    if let Some(error) = archive_refusal {
        let warning = format!(
            "The source refused to archive {}: {error:#}",
            count(not_archived, "item")
        );
        reporter.warn(&warning);
        pulled.warnings.push(warning);
    }
    Ok(pulled)
}

/// The id of the source folder named `name`, made if it is missing.
async fn source_folder(
    source: &Endpoint<'_>,
    folders: &mut HashMap<String, String>,
    name: Option<&str>,
) -> Result<Option<String>> {
    let Some(name) = name else { return Ok(None) };
    if let Some(id) = folders.get(name) {
        return Ok(Some(id.clone()));
    }
    let created = source.account.create_folder(name).await?;
    folders.insert(created.name, created.id.clone());
    Ok(Some(created.id))
}

/// Replaces the attachments `old` of `to`'s item, as this run last wrote it,
/// with copies of `from`'s. Returns the item as the server then holds it, and
/// how many files it copied.
async fn copy_attachments(
    (from, from_item): (&Endpoint<'_>, &Item),
    (to, to_item): (&Endpoint<'_>, &Item),
    old: &[String],
    verify: bool,
    mode: Mode,
) -> Result<(Item, usize)> {
    for attachment_id in old {
        to.account
            .delete_attachment(&to_item.id, attachment_id)
            .await?;
    }
    let mut copied = Vec::new();
    for attachment in &from_item.attachments {
        let data = from
            .account
            .download_attachment(&from.scope, from_item, attachment)
            .await
            .with_context(|| {
                format!(
                    "downloading attachment {} of item {}",
                    attachment.id, from_item.id
                )
            })?;
        to.account
            .upload_attachment(&to.scope, to_item, &attachment.file_name, &data)
            .await
            .with_context(|| format!("uploading an attachment to item {}", to_item.id))?;
        copied.push((
            attachment.file_name.as_str(),
            <[u8; 32]>::from(Sha256::digest(&data)),
        ));
    }
    let mut stored = to
        .account
        .get_item(&to.scope, &to_item.id)
        .await
        .with_context(|| format!("reading item {}", to_item.id))?;
    stored.collection_ids = to_item.collection_ids.clone();
    // Edited since this run wrote it: the final write would overwrite an edit
    // that no backup holds yet.
    if comparable(&stored, None, &[], mode) != comparable(to_item, None, &[], mode)
        || stored.folder_id != to_item.folder_id
    {
        bail!(
            "item {} changed during the run. Run again to copy it",
            to_item.id
        );
    }
    if verify {
        let mut found = Vec::new();
        for attachment in &stored.attachments {
            let data = to
                .account
                .download_attachment(&to.scope, &stored, attachment)
                .await?;
            found.push((
                attachment.file_name.as_str(),
                <[u8; 32]>::from(Sha256::digest(&data)),
            ));
        }
        copied.sort_unstable();
        found.sort_unstable();
        if copied != found {
            bail!(
                "the attachments of item {} do not match what was copied",
                to_item.id
            );
        }
    }
    let count = copied.len();
    Ok((stored, count))
}

/// Takes the markers off source items that the destination has a copy of,
/// as a pull leaves them. Returns how many items it rewrote.
pub async fn unmark(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    concurrency: usize,
) -> Result<usize> {
    let source_vault = source.account.load(&source.scope).await?;
    let dest_vault = dest.account.load(&dest.scope).await?;
    // A destination copy that names a source item proves the mirror paired
    // them, whatever the source item's own marker names.
    let paired: HashSet<String> = (dest_vault.items.iter())
        .filter(|item| !item.in_trash)
        .filter_map(plan::marker_of)
        .map(|marker| marker.source_id)
        .collect();
    let marked: Vec<&Item> = (source_vault.items.iter())
        .filter(|item| {
            !item.in_trash && paired.contains(&item.id) && plan::marker_of(item).is_some()
        })
        .collect();
    stream::iter(marked.iter().map(|item| async move {
        let draft = Draft {
            content: plan::unmarked(item),
            folder_id: item.folder_id.clone(),
            collection_ids: item.collection_ids.clone(),
        };
        source
            .account
            .update_item(&source.scope, item, &draft)
            .await
            .with_context(|| format!("updating source item {}", item.id))
    }))
    .buffer_unordered(concurrency.max(1))
    .try_collect::<Vec<_>>()
    .await?;
    Ok(marked.len())
}

fn ensure_distinct(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    source_vault: &Vault,
    dest_vault: &Vault,
) -> Result<()> {
    if source.account.api_url() == dest.account.api_url()
        && source.account.user_id == dest.account.user_id
        && source.scope == dest.scope
    {
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
    /// False when the destination account cannot take attachments.
    copy_attachments: bool,
    /// The destination as already read, if it was.
    dest_vault: Option<Vault>,
}

impl Run<'_> {
    fn mode(&self) -> Result<Mode> {
        mode(&self.source.scope, &self.dest.scope)
    }

    /// Removes source items the destination could not hold, with a warning.
    fn leave_out(&mut self, source: &mut Vault, unfit: impl Fn(&Item) -> bool, reason: &str) {
        let left_out = source
            .items
            .iter()
            .filter(|item| !item.in_trash && unfit(item))
            .count();
        source.items.retain(|item| !unfit(item));
        if left_out > 0 {
            self.warn(format!(
                "Left {} out of the mirror because {reason}.",
                count(left_out, "item")
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
        match unmatched.as_slice() {
            [] => {}
            [name] => self.warn(format!(
                "The filter name {name} matches no {kind} in the source."
            )),
            names => self.warn(format!(
                "The filter names {} match no {kind} in the source.",
                names.join(", ")
            )),
        }
        self.reporter.status(
            "Filtered",
            &format!("{after} of {} by {kind}", count(before, "item")),
        );
    }

    fn warn(&mut self, message: String) {
        self.reporter.warn(&message);
        self.warnings.push(message);
    }

    async fn execute(&mut self) -> Result<bool> {
        let mode = self.mode()?;
        let loaded = self.dest_vault.take();
        let (source_vault, dest_vault) = tokio::try_join!(
            async {
                self.source
                    .account
                    .load(&self.source.scope)
                    .await
                    .context("reading the source vault")
            },
            async {
                match loaded {
                    Some(vault) => Ok(vault),
                    None => self
                        .dest
                        .account
                        .load(&self.dest.scope)
                        .await
                        .context("reading the destination vault"),
                }
            },
        )?;
        ensure_distinct(self.source, self.dest, &source_vault, &dest_vault)?;

        self.stage = "planning";
        let mut source_vault = source_vault;
        self.filter(&mut source_vault, mode);
        // Without these, items the destination hides could be created again
        // on every run.
        let traits = self
            .dest
            .account
            .server_traits()
            .await
            .context("reading the destination server's settings")?;
        self.leave_out(
            &mut source_vault,
            |item| {
                item.item_type()
                    .is_some_and(|kind| traits.hidden_item_types.contains(&kind))
            },
            "the destination server does not show bank accounts, driver's licences or passports yet",
        );
        // Organizations pay for attachments themselves.
        self.copy_attachments = mode == Mode::Organization || self.dest.account.premium;
        if mode == Mode::Organization && traits.items_need_collections {
            self.leave_out(
                &mut source_vault,
                |item| item.collection_ids.is_empty(),
                "they are in no collection, which the destination server requires",
            );
        }

        self.stage = "planning";
        let plan = plan::plan_with(&source_vault, &dest_vault, mode, self.copy_attachments);
        if plan.attachments_withheld > 0 {
            self.warn(format!(
                "Not copying {}: the destination account has no premium subscription, which \
                 Bitwarden requires for attachments. Everything else is mirrored, and the \
                 attachments follow once the account has premium.",
                count(plan.attachments_withheld, "attachment")
            ));
        }
        let summary = plan.summary(&source_vault);
        self.summary = Some(summary);
        let (verb, plan_text) = describe_plan(&summary, self.options.dry_run);
        self.reporter.status(verb, &plan_text);
        if plan.reversed > 0 && !plan.untracked.is_empty() {
            self.warn(format!(
                "The destination has {} that no source item was copied from. Runs leave them as \
                 they are, so if you deleted them in the source, delete them here too.",
                count(plan.untracked.len(), "item")
            ));
        } else if plan.tracked_before == 0 && !plan.untracked.is_empty() {
            self.warn(format!(
                "The destination has {} that vault-mirror didn't make. Runs leave them as they \
                 are, so if they are older copies of the source, delete them to avoid duplicates.",
                count(plan.untracked.len(), "item")
            ));
        }
        if plan.detached > 0 {
            self.warn(format!(
                "The destination has {} with the vault-mirror field of another copy, changed \
                 since, such as a cloned copy. Runs leave {them} alone but delete {them} with \
                 the source item. To keep {them} for good, remove the vault-mirror field.",
                count(plan.detached, "item"),
                them = if plan.detached == 1 { "it" } else { "them" },
            ));
        }
        let changed = plan.updates.iter().filter(|u| u.changed_in_dest()).count();
        if changed > 0 {
            let them = if changed == 1 { "it" } else { "them" };
            let kept = match self.options.dest_backed_up {
                true => " Your changes are in the destination backup from the start of this run.",
                false => "",
            };
            self.warn(format!(
                "{} changed in the destination since vault-mirror wrote {them}. A run replaces \
                 {them} with the source version, so make lasting changes in the source.{kept}",
                count(changed, "copy")
            ));
        }
        plan::check_safety(&plan, &self.options.safety)?;
        if self.options.dry_run || !plan.has_changes() {
            return Ok(false);
        }

        self.apply(&plan, &source_vault, &dest_vault, mode).await?;

        self.stage = "verifying the destination";
        self.verify(&source_vault, mode).await?;
        self.reporter.status("Verified", "the destination");
        // Nothing changed if the plan only deleted a folder or collection that
        // has gained an item since.
        let done = self.summary.unwrap_or_default();
        let nothing = Summary {
            items_unchanged: done.items_unchanged,
            ..Summary::default()
        };
        Ok(done != nothing)
    }

    async fn apply(
        &mut self,
        plan: &Plan,
        source_vault: &Vault,
        dest_vault: &Vault,
        mode: Mode,
    ) -> Result<()> {
        let source = self.source;
        let dest = self.dest;
        let concurrency = self.options.concurrency.max(1);

        self.stage = "creating folders and collections";
        let mut folder_ids = plan.folder_ids.clone();
        for name in &plan.folders_to_create {
            let folder = dest.account.create_folder(name).await?;
            folder_ids.insert(folder.name, folder.id);
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

        self.stage = "writing items";
        // Each write yields the entry it was for and the destination item as
        // the server now holds it.
        // An item whose attachments are still to be copied or removed gets a
        // pending marker first, and its final one once they are done and
        // checked. A run that stops in between leaves an item the next run
        // repairs.
        let pending = |entry_index: usize| {
            plan.copy_attachments
                && !source_vault.items[plan.entries[entry_index].source]
                    .attachments
                    .is_empty()
        };
        let drafts = plan
            .creates
            .iter()
            .map(|&entry_index| {
                let entry = &plan.entries[entry_index];
                match pending(entry_index) {
                    true => entry.pending_draft(source_vault, &folder_ids, &collection_ids),
                    false => entry.draft(source_vault, &folder_ids, &collection_ids),
                }
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
            .create_items(&dest.scope, &drafts, &folder_names, concurrency, &|n| {
                writes.add(n)
            })
            .await?;
        let created: Vec<(usize, Item)> = plan.creates.iter().copied().zip(new_items).collect();

        let updated: Vec<(usize, Item)> =
            stream::iter(plan.updates.iter().enumerate().map(|(index, update)| {
                let (folder_ids, collection_ids, writes) = (&folder_ids, &collection_ids, &writes);
                async move {
                    let existing = &dest_vault.items[update.dest];
                    let entry = &plan.entries[update.entry];
                    let draft = if update.sync_attachments {
                        entry.pending_draft(source_vault, folder_ids, collection_ids)?
                    } else if update.write_content {
                        entry.draft(source_vault, folder_ids, collection_ids)?
                    } else {
                        return anyhow::Ok((index, existing.clone()));
                    };
                    let item = dest
                        .account
                        .update_item(&dest.scope, existing, &draft)
                        .await
                        .with_context(|| format!("updating destination item {}", existing.id))?;
                    writes.add(1);
                    Ok((index, item))
                }
            }))
            .buffer_unordered(concurrency)
            .try_collect()
            .await?;
        if !created.is_empty() {
            self.reporter
                .status("Created", &count(created.len(), "item"));
        }
        if rewritten > 0 {
            self.reporter.status("Updated", &count(rewritten, "item"));
        }

        self.stage = "copying attachments";
        // (entry, destination item, destination attachments to remove first)
        let mut transfers: Vec<(usize, &Item, Vec<String>)> = Vec::new();
        for (entry_index, item) in &created {
            if pending(*entry_index) {
                transfers.push((*entry_index, item, Vec::new()));
            }
        }
        for (index, item) in &updated {
            let update = &plan.updates[*index];
            if update.sync_attachments {
                let old = dest_vault.items[update.dest]
                    .attachments
                    .iter()
                    .map(|a| a.id.clone())
                    .collect();
                transfers.push((update.entry, item, old));
            }
        }
        let verify_attachments = self.options.verify_attachments;
        let uploads = Tally::new(
            self.reporter,
            "Copying",
            transfers
                .iter()
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
                let (folder_ids, collection_ids, uploads) =
                    (&folder_ids, &collection_ids, &uploads);
                async move {
                    let entry = &plan.entries[entry_index];
                    let source_item = &source_vault.items[entry.source];
                    let (stored, copied) = copy_attachments(
                        (source, source_item),
                        (dest, dest_item),
                        &old,
                        verify_attachments,
                        mode,
                    )
                    .await?;
                    uploads.add(copied);
                    let draft =
                        entry.finished_draft(source_vault, &stored, folder_ids, collection_ids)?;
                    let finished = dest
                        .account
                        .update_item(&dest.scope, &stored, &draft)
                        .await
                        .with_context(|| format!("updating destination item {}", dest_item.id))?;
                    anyhow::Ok((entry_index, copied, finished))
                }
            }))
            // Attachments are held in memory while they are re-encrypted.
            .buffer_unordered(concurrency.min(2))
            .try_collect()
            .await?;
        let mut finished: HashMap<usize, Item> = HashMap::new();
        let mut total = 0;
        for (entry_index, copied, item) in copied {
            total += copied;
            finished.insert(entry_index, item);
        }
        if total > 0 {
            self.reporter.status("Copied", &count(total, "attachment"));
        }

        self.stage = "setting archive state";
        let mut to_archive: Vec<String> = Vec::new();
        let mut to_unarchive: Vec<String> = Vec::new();
        // Decided from what the server reports after the writes, not from the
        // plan: servers differ in what a write, or an attachment upload, does
        // to an item's archive state.
        let written = created
            .iter()
            .map(|(entry_index, item)| (*entry_index, item))
            .chain(
                updated
                    .iter()
                    .map(|(index, item)| (plan.updates[*index].entry, item)),
            )
            .map(|(entry_index, item)| (entry_index, finished.get(&entry_index).unwrap_or(item)));
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
                // Archiving is a paid feature on some servers and missing on
                // older ones. The items are mirrored either way, so a refusal
                // does not fail the run; any other failure does.
                if !refused(&error) {
                    return Err(error);
                }
                self.archive_unsupported = true;
                self.warn(format!(
                    "The destination refused to change the archive state of {}: {error:#}",
                    count(ids.len(), "item")
                ));
            }
        }

        // Deleting items comes last, so a run that fails before has removed
        // no item, only attachments it was replacing.
        self.stage = "deleting obsolete items";
        let ids: Vec<String> = plan
            .deletes
            .iter()
            .map(|delete| dest_vault.items[delete.dest].id.clone())
            .collect();
        if !ids.is_empty() {
            dest.account.delete_items(&ids).await?;
            self.reporter.status("Deleted", &count(ids.len(), "item"));
        }
        if plan.folders_to_delete.is_empty() && plan.collections_to_delete.is_empty() {
            return Ok(());
        }
        // The plan may come from the backup at the start of the run. Someone
        // may have put an item in a folder or collection since, so it is
        // deleted only if a plan from the destination as it is now agrees.
        self.stage = "deleting obsolete folders and collections";
        let now = dest.account.load(&dest.scope).await?;
        let fresh = plan::plan_with(source_vault, &now, mode, self.copy_attachments);
        let mut kept = Summary::default();
        for folder in &plan.folders_to_delete {
            match fresh.folders_to_delete.contains(folder) {
                true => dest.account.delete_folder(&folder.id).await?,
                false => kept.folders_deleted += 1,
            }
        }
        if let Scope::Organization(org) = &dest.scope {
            for collection in &plan.collections_to_delete {
                match fresh.collections_to_delete.contains(collection) {
                    true => dest.account.delete_collection(org, &collection.id).await?,
                    false => kept.collections_deleted += 1,
                }
            }
        }
        if let Some(summary) = &mut self.summary {
            summary.folders_deleted -= kept.folders_deleted;
            summary.collections_deleted -= kept.collections_deleted;
        }
        Ok(())
    }

    /// Reads the destination again and checks that nothing is left to do.
    /// Attachments were checked as they were copied.
    async fn verify(&mut self, source_vault: &Vault, mode: Mode) -> Result<()> {
        let dest = self.dest;
        let dest_vault = dest.account.load(&dest.scope).await?;
        let mut again = plan::plan_with(source_vault, &dest_vault, mode, self.copy_attachments);
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

/// True when the server answered that it does not accept the request, as
/// opposed to failing to answer, limiting the rate, or failing itself.
pub(crate) fn refused(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiError>())
        .and_then(ApiError::status)
        .is_some_and(|status| matches!(status.as_u16(), 400 | 402 | 403 | 404 | 405 | 501))
}

/// The plan's changes in a few words, leaving out what does not happen, as
/// a verb and the rest of the sentence.
fn describe_plan(summary: &Summary, dry_run: bool) -> (&'static str, String) {
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
    .map(|(n, verb, noun)| format!("{verb} {}", count(n, noun)))
    .collect();
    let unchanged = format!("{} unchanged", count(summary.items_unchanged, "item"));
    let changes = changes.join(", ");
    match (changes.is_empty(), dry_run) {
        (true, false) => ("Planned", format!("no changes; {unchanged}")),
        (true, true) => ("Would", format!("change nothing; {unchanged}")),
        (false, false) => ("Planned", format!("to {changes}; {unchanged}")),
        (false, true) => ("Would", format!("{changes}; {unchanged}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{Collection, Folder};

    #[test]
    fn only_a_refusal_counts_as_archiving_being_unsupported() {
        let answered = |status: u16| {
            anyhow::Error::new(ApiError::Status {
                context: "archiving items".into(),
                status: reqwest::StatusCode::from_u16(status).unwrap(),
                message: String::new(),
                retry_after: None,
            })
            .context("setting archive state")
        };
        for status in [400, 402, 403, 404, 405, 501] {
            assert!(refused(&answered(status)), "{status}");
        }
        for status in [401, 409, 429, 500, 502, 503] {
            assert!(!refused(&answered(status)), "{status}");
        }
        assert!(!refused(&anyhow::anyhow!("connection reset")));
    }

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
    fn plans_are_described_by_what_changes() {
        let summary = Summary {
            items_created: 9,
            items_unchanged: 1,
            folders_created: 1,
            attachments_copied: 4,
            ..Summary::default()
        };
        let changes = "create 9 items, create 1 folder, copy 4 attachments; 1 item unchanged";
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
