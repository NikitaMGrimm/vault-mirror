//! Makes a destination vault match a source vault.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use sha2::{Digest, Sha256};

use crate::{
    api::ApiError,
    plan::{self, DeleteReason, Mode, Plan, Safety, Summary, comparable, differing_paths},
    text::count,
    vault::{Account, Draft, Item, Scope, ServerTraits, Vault},
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
    /// Full source states captured before the forward mirror, for guarded
    /// marker cleanup after a pull.
    pub cleanup_source_items: Vec<Item>,
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
    mirror_after_pull(
        source,
        dest,
        dest_vault,
        options,
        reporter,
        &BTreeSet::new(),
    )
    .await
}

/// Mirrors after a successful pull, allowing only its verified source items
/// to finish synchronizing their attachment markers.
pub async fn mirror_after_pull(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    dest_vault: Option<Vault>,
    options: &MirrorOptions,
    reporter: &dyn Reporter,
    verified_source_ids: &BTreeSet<String>,
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
        verified_source_ids: verified_source_ids.clone(),
        cleanup_source_items: Vec::new(),
    };
    match run.execute().await {
        Ok(changed) => Ok(Outcome {
            summary: run.summary.unwrap_or_default(),
            changed,
            warnings: run.warnings,
            cleanup_source_items: run.cleanup_source_items,
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
    /// The source items `unmark` may take the markers off once the mirror
    /// after the pull has run: those this pull wrote, and those that an
    /// earlier pull finished and the filter selects.
    pub cleanup_source_ids: BTreeSet<String>,
}

/// Why a pull cannot bring back a destination item as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unpullable {
    /// The filter leaves out the item or its source item, so the pull must not
    /// write it, and the mirror after it would delete or replace it.
    Filtered,
    /// A copy whose source item is deleted or in the trash, so the mirror
    /// after the pull would delete it.
    SourceGone,
    /// The source server would not show the item's type.
    HiddenType,
    /// The source server requires a collection, and the filter selects none.
    NoCollection,
    /// The source account cannot take its attachments.
    NoPremium,
}

impl Unpullable {
    fn reason(self, mode: Mode) -> &'static str {
        match (self, mode) {
            (Self::Filtered, Mode::Personal) => {
                "INCLUDE_FOLDERS or EXCLUDE_FOLDERS leaves out them or their source items"
            }
            (Self::Filtered, Mode::Organization) => {
                "INCLUDE_COLLECTIONS or EXCLUDE_COLLECTIONS leaves out them or their source items"
            }
            (Self::SourceGone, _) => "their source items are deleted or in the trash",
            (Self::HiddenType, _) => {
                "they are bank accounts, driver's licences or passports, which the source server \
                 does not show yet"
            }
            (Self::NoCollection, _) => {
                "they are in no collection the filter selects, which the source server requires"
            }
            (Self::NoPremium, _) => {
                "they have attachments, and the source account has no premium subscription, \
                 which Bitwarden requires for attachments"
            }
        }
    }
}

fn refuse<'a>(
    unpullable: &mut BTreeMap<Unpullable, Vec<&'a str>>,
    why: Unpullable,
    item: &'a Item,
) {
    unpullable.entry(why).or_default().push(&item.id);
}

/// Up to five ids, and how many more there are.
fn listed(ids: &[&str]) -> String {
    match ids.len() {
        0..=5 => ids.join(", "),
        n => format!("{} and {} more", ids[..5].join(", "), n - 5),
    }
}

/// Copies into the source what changed in the destination since vault-mirror
/// wrote it, for when the destination was used while the source was down.
/// Edited copies replace their originals, and items vault-mirror didn't make
/// are added. Nothing is deleted. The vaults are the ones backed up, and every
/// write names the revision it replaces, so the server refuses to overwrite a
/// source edit made since. `backed_up` says whether a backup holds what this
/// replaces.
///
/// The filter bounds every write. Before the first one, the pull checks that
/// it can bring back every changed destination item that the mirror after it
/// would replace or delete, and changes nothing if it cannot. An added item
/// that the source cannot take is left out with a warning instead, as the
/// mirror leaves it alone.
///
/// Each item it writes gets a marker naming the destination item, so that the
/// mirror after it pairs the two even when it is interrupted and run again.
/// The pull reads each item back, as a server can drop part of a write.
/// `unmark` removes the markers once the mirror has run.
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
    // The mirror checks this too, but only after the pull has changed the
    // source.
    let mut selected_source = source_vault.clone();
    options.filter.apply(&mut selected_source, mode);
    plan::check_safety(
        &plan::plan(&selected_source, dest_vault, mode),
        &options.safety,
    )?;
    let plan = plan::plan(source_vault, dest_vault, mode);
    let mut selected_dest = dest_vault.clone();
    options.filter.apply(&mut selected_dest, mode);
    // Destination items as the filter selects them, with only the selected
    // collections.
    let projected: HashMap<&str, &Item> = (selected_dest.items.iter())
        .map(|item| (item.id.as_str(), item))
        .collect();
    let in_source: HashSet<&str> = (selected_source.items.iter())
        .map(|item| item.id.as_str())
        .collect();
    let mut source_folders: HashMap<String, String> = (source_vault.folders.iter())
        .map(|f| (f.name.clone(), f.id.clone()))
        .collect();
    let mut source_collections: HashMap<String, String> = (source_vault.collections.iter())
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();
    let traits = source
        .account
        .server_traits()
        .await
        .context("reading the source server's settings")?;
    let mut unpullable: BTreeMap<Unpullable, Vec<&str>> = BTreeMap::new();

    // Source items that name a destination item come from an earlier pull.
    let mut linked = HashMap::new();
    for item in source_vault.items.iter().filter(|item| !item.in_trash) {
        if let Some(marker) = plan::marker_of(item)
            && let Some(previous) = linked.insert(marker.source_id.clone(), item)
            && (marker.files_pending()
                || plan::marker_of(previous).is_some_and(|m| m.files_pending()))
        {
            bail!(
                "source items {} and {} both name destination item {}; reconcile their markers before pulling, so the pull changed nothing",
                previous.id,
                item.id,
                marker.source_id
            );
        }
    }
    // Items vault-mirror didn't make, each with its source item if an earlier
    // pull made one but stopped before it finished.
    let mut added: Vec<(&Item, Option<&Item>)> = Vec::new();
    let mut halfway: HashSet<&str> = HashSet::new();
    for item in
        (dest_vault.items.iter()).filter(|item| !item.in_trash && plan::marker_of(item).is_none())
    {
        let selected = projected.contains_key(item.id.as_str());
        let Some(&made) = linked.get(&item.id) else {
            if selected {
                added.push((item, None));
            }
            continue;
        };
        if !plan::marker_of(made).is_some_and(|marker| marker.files_pending()) {
            continue;
        }
        halfway.insert(item.id.as_str());
        // Finishing it writes to the source item. Leaving it lets the mirror
        // pair the two and replace the destination item with the unfinished
        // source item.
        match (selected, in_source.contains(made.id.as_str())) {
            (true, true) => added.push((item, Some(made))),
            (false, false) => {}
            _ => refuse(&mut unpullable, Unpullable::Filtered, item),
        }
    }

    // Without these, an item the source hides would be added again on every
    // pull, and one it refuses would stop every pull. A resumed item cannot
    // be left out, as the mirror would replace it with its unfinished source
    // item.
    let mut left_out = 0;
    let mut uploads: Vec<&Item> = Vec::new();
    let mut additions = Vec::new();
    for (item, made) in added {
        let kept = kept_by_resumption(dest_vault, item, made, mode);
        let homeless = mode == Mode::Organization
            && traits.items_need_collections
            && collection_names(&selected_dest, projected[item.id.as_str()], mode).is_empty();
        let unfit = match kept {
            Some(_) => None,
            None if traits.hides(item) => Some(Unpullable::HiddenType),
            None if homeless => Some(Unpullable::NoCollection),
            None => None,
        };
        match (unfit, made) {
            (Some(_), None) => left_out += 1,
            (Some(why), Some(_)) => refuse(&mut unpullable, why, item),
            (None, _) => {
                if !item.attachments.is_empty() {
                    uploads.push(item);
                }
                additions.push((item, made, kept));
            }
        }
    }
    let mut pulled = Pulled::default();
    if left_out > 0 {
        let warning = format!(
            "Left {} out of the pull because the source server would not show them: bank \
             accounts, driver's licences and passports are not shown yet, and organization \
             items must be in a collection.",
            count(left_out, "item")
        );
        reporter.warn(&warning);
        pulled.warnings.push(warning);
    }

    let changed: Vec<&plan::Update> = (plan.updates.iter())
        .filter(|update| {
            update.changed_in_dest() && !halfway.contains(dest_vault.items[update.dest].id.as_str())
        })
        .collect();
    for update in &changed {
        let original = &source_vault.items[plan.entries[update.entry].source];
        let copy = &dest_vault.items[update.dest];
        // In an individual vault, an edit brings the copy's folder along, and
        // with it the original.
        let moves = mode == Mode::Personal && update.content_changed_in_dest;
        if !in_source.contains(original.id.as_str())
            || (moves && !projected.contains_key(copy.id.as_str()))
        {
            refuse(&mut unpullable, Unpullable::Filtered, copy);
        } else if update.content_changed_in_dest && traits.hides(copy) {
            refuse(&mut unpullable, Unpullable::HiddenType, copy);
        }
        if update.files_changed_in_dest && !copy.attachments.is_empty() {
            uploads.push(copy);
        }
    }
    for delete in plan.deletes.iter().filter(|delete| delete.changed_in_dest) {
        let copy = &dest_vault.items[delete.dest];
        refuse(&mut unpullable, Unpullable::SourceGone, copy);
    }
    // Organizations pay for attachments themselves.
    if mode == Mode::Personal && !source.account.premium {
        for item in uploads {
            refuse(&mut unpullable, Unpullable::NoPremium, item);
        }
    }
    if !unpullable.is_empty() {
        let total: usize = unpullable.values().map(Vec::len).sum();
        let reasons: Vec<String> = (unpullable.iter())
            .map(|(why, ids)| format!("{} ({})", why.reason(mode), listed(ids)))
            .collect();
        bail!(
            "the pull cannot bring back {} as {}, so it changed nothing: {}. Fix that, or copy \
             them into the source by hand, and run again",
            count(total, "destination item"),
            if total == 1 { "it is" } else { "they are" },
            reasons.join("; ")
        );
    }

    let both = (changed.iter())
        .filter(|update| plan.changed_in_both(update, dest_vault))
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
    pulled.summary.items_created = additions.len();
    let items: Vec<String> = [
        (additions.len(), "new item"),
        (changed.len(), "changed item"),
    ]
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
    pulled.cleanup_source_ids = (selected_source.items.iter())
        .filter(|item| {
            !item.in_trash && plan::marker_of(item).is_some_and(|marker| !marker.files_pending())
        })
        .map(|item| item.id.clone())
        .collect();

    let mut archiver = Archiver::default();
    let changing: HashSet<&str> = changed
        .iter()
        .map(|update| {
            source_vault.items[plan.entries[update.entry].source]
                .id
                .as_str()
        })
        .collect();
    for original in &selected_source.items {
        let Some(marker) = plan::marker_of(original).filter(|marker| marker.files_pending()) else {
            continue;
        };
        if changing.contains(original.id.as_str()) {
            continue;
        }
        let Some(copy) = dest_vault
            .items
            .iter()
            .find(|item| !item.in_trash && item.id == marker.source_id)
        else {
            continue;
        };
        let folder = folder_name(dest_vault, copy, mode);
        let names = collection_names(dest_vault, copy, mode);
        let draft = Draft {
            content: original.content.clone(),
            folder_id: original.folder_id.clone(),
            collection_ids: original.collection_ids.clone(),
        };
        let files = attachment_ids(original)
            .into_iter()
            .map(str::to_string)
            .collect();
        check_pulled(
            source,
            original,
            &draft,
            &files,
            original.archived,
            (copy, folder.as_deref(), &names),
            mode,
        )
        .await?;
        pulled.cleanup_source_ids.insert(original.id.clone());
    }
    for update in changed {
        let original = &source_vault.items[plan.entries[update.entry].source];
        let copy = &dest_vault.items[update.dest];
        let folder = folder_name(dest_vault, copy, mode);
        let names = collection_names(dest_vault, copy, mode);
        let (content, folder_id) = match (update.content_changed_in_dest, mode) {
            (true, Mode::Personal) => (
                copy,
                source_folder(source, &mut source_folders, folder.as_deref()).await?,
            ),
            (true, Mode::Organization) => (copy, original.folder_id.clone()),
            (false, _) => (original, original.folder_id.clone()),
        };
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
        pulled.cleanup_source_ids.insert(original.id.clone());
        let mut written = source
            .account
            .update_item(&source.scope, original, &draft(None))
            .await
            .with_context(|| format!("updating source item {}", original.id))?;
        let mut final_draft = draft(files);
        let mut wanted_files = attachment_ids(original)
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut wanted_archived = original.archived;
        if update.files_changed_in_dest {
            let old: Vec<String> = original.attachments.iter().map(|a| a.id.clone()).collect();
            let (mut stored, copied) =
                copy_attachments((dest, copy), (source, &written), &old, true, mode).await?;
            pulled.summary.attachments_copied += copied;
            // Bitwarden's server takes an item out of the archive on an upload.
            if original.archived && !stored.archived {
                stored = archiver.archive(source, stored, mode).await?;
                wanted_archived = archiver.refusal.is_none();
            }
            wanted_files = attachment_ids(&stored)
                .into_iter()
                .map(str::to_string)
                .collect();
            final_draft = draft(Some(&stored));
            written = stored;
        }
        let pair = (copy, folder.as_deref(), names.as_slice());
        check_pulled(
            source,
            &written,
            &final_draft,
            &wanted_files,
            wanted_archived,
            pair,
            mode,
        )
        .await?;
        written = source
            .account
            .update_item(&source.scope, &written, &final_draft)
            .await?;
        check_pulled(
            source,
            &written,
            &final_draft,
            &wanted_files,
            wanted_archived,
            pair,
            mode,
        )
        .await?;
    }

    for (item, made, kept) in additions {
        let folder = folder_name(dest_vault, item, mode);
        let names = collection_names(dest_vault, item, mode);
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
                    let selected = projected[item.id.as_str()];
                    for name in collection_names(&selected_dest, selected, mode) {
                        if !source_collections.contains_key(&name) {
                            let created =
                                source.account.create_collection(org, &name, None).await?;
                            source_collections.insert(created.name, created.id);
                        }
                        collection_ids.push(source_collections[&name].clone());
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
            Some(made) => {
                source
                    .account
                    .update_item(&source.scope, made, &draft(None))
                    .await?
            }
            None => {
                source
                    .account
                    .create_item(&source.scope, &draft(None))
                    .await?
            }
        };
        pulled.cleanup_source_ids.insert(made.id.clone());
        let old: Vec<String> = made.attachments.iter().map(|a| a.id.clone()).collect();
        let (mut stored, copied) =
            copy_attachments((dest, item), (source, &made), &old, true, mode).await?;
        pulled.summary.attachments_copied += copied;
        let mut wanted_archived = stored.archived;
        // Archived before the final marker, so that a pull that stops in
        // between finishes the item, archive state included, when run again.
        if item.archived && !stored.archived {
            stored = archiver.archive(source, stored, mode).await?;
            wanted_archived = archiver.refusal.is_none();
        }
        let final_draft = draft(Some(&stored));
        let wanted = attachment_ids(&stored)
            .into_iter()
            .map(str::to_string)
            .collect();
        let pair = (item, folder.as_deref(), names.as_slice());
        check_pulled(
            source,
            &stored,
            &final_draft,
            &wanted,
            wanted_archived,
            pair,
            mode,
        )
        .await?;
        let written = source
            .account
            .update_item(&source.scope, &stored, &final_draft)
            .await
            .with_context(|| format!("updating source item {}", stored.id))?;
        check_pulled(
            source,
            &written,
            &final_draft,
            &wanted,
            wanted_archived,
            pair,
            mode,
        )
        .await?;
    }
    if let Some(error) = archiver.refusal {
        let warning = format!(
            "The source refused to archive {}: {error:#}",
            count(archiver.not_archived, "item")
        );
        reporter.warn(&warning);
        pulled.warnings.push(warning);
    }
    Ok(pulled)
}

/// The source item an earlier pull made from `item` and stopped before
/// finishing, if the pull keeps what changed in it since: it does unless
/// `item` changed too.
fn kept_by_resumption<'a>(
    dest_vault: &Vault,
    item: &Item,
    made: Option<&'a Item>,
    mode: Mode,
) -> Option<&'a Item> {
    let folder = folder_name(dest_vault, item, mode);
    let names = collection_names(dest_vault, item, mode);
    made.filter(|made| {
        plan::marker_of(made)
            .is_some_and(|marker| marker.content_matches(item, folder.as_deref(), &names, mode))
    })
}

/// Reads back a source item the pull finished, and checks that the server
/// holds what the pull wrote: `draft`'s content and folder, and the
/// attachments `files`. A server can accept a write and drop part of it, and
/// the mirror after the pull would copy the loss into the destination.
///
/// On a mismatch the item gets a pending marker naming the destination item
/// again, which stops a run without `--pull`. The marker records what the
/// server holds, so that the next pull writes the destination item again
/// instead of taking that for an edit made in the source.
async fn check_pulled(
    source: &Endpoint<'_>,
    written: &Item,
    draft: &Draft,
    files: &BTreeSet<String>,
    archived: bool,
    (copy, folder, names): (&Item, Option<&str>, &[String]),
    mode: Mode,
) -> Result<()> {
    let vault = source
        .account
        .load(&source.scope)
        .await
        .with_context(|| format!("checking source item {} in the vault", written.id))?;
    let held = vault
        .items
        .iter()
        .find(|item| item.id == written.id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "source item {} is missing after the pull; the destination is unchanged",
                written.id
            )
        })?;
    let wanted = Item {
        content: draft.content.clone(),
        ..held.clone()
    };
    let mut held_collections = held.collection_ids.clone();
    let mut wanted_collections = draft.collection_ids.clone();
    held_collections.sort();
    wanted_collections.sort();
    let held_files: BTreeSet<String> = held.attachments.iter().map(|a| a.id.clone()).collect();
    if comparable(held, None, &[], mode) == comparable(&wanted, None, &[], mode)
        && held.folder_id == draft.folder_id
        && held_collections == wanted_collections
        && !held.in_trash
        && held.archived == archived
        && held_files == *files
    {
        return Ok(());
    }
    let holds = Item {
        id: copy.id.clone(),
        ..held.clone()
    };
    let pending = Draft {
        content: plan::mark_copy(held, &holds, folder, names, mode, None),
        folder_id: held.folder_id.clone(),
        collection_ids: held.collection_ids.clone(),
    };
    (source.account.update_item(&source.scope, held, &pending))
        .await
        .with_context(|| format!("marking source item {} as unfinished", held.id))?;
    bail!(
        "source item {} holds only part of what the pull wrote into it from destination item {}, \
         so the source server may not support all of it. The destination item is unchanged, \
         and a run without --pull stops at it",
        held.id,
        copy.id
    );
}

/// Archives source items again after a pull wrote them, and counts those the
/// server refuses to archive.
#[derive(Default)]
struct Archiver {
    refusal: Option<anyhow::Error>,
    not_archived: usize,
}

impl Archiver {
    /// Returns `stored` as the server holds it once archived, or unchanged
    /// if the server refuses, now or earlier in the pull.
    async fn archive(&mut self, source: &Endpoint<'_>, stored: Item, mode: Mode) -> Result<Item> {
        if self.refusal.is_some() {
            self.not_archived += 1;
            return Ok(stored);
        }
        match source
            .account
            .set_archived(std::slice::from_ref(&stored.id), true)
            .await
        {
            Ok(()) => {
                let mut archived = source.account.get_item(&source.scope, &stored.id).await?;
                archived.collection_ids = stored.collection_ids.clone();
                if comparable(&archived, None, &[], mode) != comparable(&stored, None, &[], mode)
                    || archived.folder_id != stored.folder_id
                    || attachment_ids(&archived) != attachment_ids(&stored)
                {
                    bail!(
                        "item {} changed during the run. Run again to copy it",
                        stored.id
                    );
                }
                Ok(archived)
            }
            Err(error) if refused(&error) => {
                self.not_archived += 1;
                self.refusal = Some(error);
                Ok(stored)
            }
            Err(error) => Err(error),
        }
    }
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
/// with copies of `from`'s. The copies are uploaded and checked before `old`
/// is deleted, so a failed upload leaves the files it was replacing. Returns
/// the item as the server then holds it, and how many files it copied.
async fn copy_attachments(
    (from, from_item): (&Endpoint<'_>, &Item),
    (to, to_item): (&Endpoint<'_>, &Item),
    old: &[String],
    verify: bool,
    mode: Mode,
) -> Result<(Item, usize)> {
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
        // Checked whatever `verify` says before the files they replace go.
        let read = !old.is_empty() || verify;
        let uploaded = reread(to, to_item, mode).await?;
        for (id, name, digest) in &copied {
            let found = uploaded.attachments.iter().find(|a| a.id == *id);
            let matches = match found {
                Some(attachment) if attachment.file_name == *name => {
                    !read || {
                        let data = to
                            .account
                            .download_attachment(&to.scope, &uploaded, attachment)
                            .await?;
                        <[u8; 32]>::from(Sha256::digest(&data)) == *digest
                    }
                }
                _ => false,
            };
            if !matches {
                bail!(
                    "the attachments of item {} do not match what was copied",
                    to_item.id
                );
            }
        }
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

/// `item` as the server now holds it. Fails if it was edited since this run
/// wrote it: the final write would overwrite an edit that no backup holds yet.
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

/// Takes the markers off the captured source items that the destination has
/// a copy of. The caller must persist a recovery hold before calling this,
/// and release it only on success. An interrupted write can remove a marker
/// before verification can check what the server kept.
pub async fn unmark(
    source: &Endpoint<'_>,
    dest: &Endpoint<'_>,
    expected: &[Item],
    concurrency: usize,
) -> Result<usize> {
    let mode = mode(&source.scope, &dest.scope)?;
    let source_vault = source.account.load(&source.scope).await?;
    let dest_vault = dest.account.load(&dest.scope).await?;
    // A destination copy that names a source item proves the mirror paired
    // them, whatever the source item's own marker names.
    let paired: HashSet<String> = (dest_vault.items.iter())
        .filter(|item| !item.in_trash)
        .filter_map(plan::marker_of)
        .map(|marker| marker.source_id)
        .collect();
    let mut marked = Vec::new();
    for item in expected
        .iter()
        .filter(|item| plan::marker_of(item).is_some())
    {
        let held = source_vault.items.iter().find(|held| held.id == item.id);
        if !paired.contains(&item.id) || held.is_none_or(|held| !same_state(held, item)) {
            bail!(
                "source item {} changed after the forward mirror; marker cleanup changed nothing",
                item.id
            );
        }
        marked.push(item);
    }
    // Verification needs every file's bytes from before the first rewrite.
    let files: Vec<BTreeMap<String, [u8; 32]>> =
        stream::iter(marked.iter().map(|item| async move {
            let mut hashes = BTreeMap::new();
            for attachment in &item.attachments {
                let data = source
                    .account
                    .download_attachment(&source.scope, item, attachment)
                    .await?;
                hashes.insert(attachment.id.clone(), Sha256::digest(&data).into());
            }
            anyhow::Ok(hashes)
        }))
        .buffered(concurrency.max(1))
        .try_collect()
        .await?;
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
    let vault = source
        .account
        .load(&source.scope)
        .await
        .context("checking marker cleanup in the source vault")?;
    for (item, hashes) in marked.iter().zip(files) {
        let held = vault
            .items
            .iter()
            .find(|held| held.id == item.id)
            .ok_or_else(|| {
                anyhow::anyhow!("source item {} is missing after marker cleanup", item.id)
            })?;
        let mut collections = item.collection_ids.clone();
        let mut held_collections = held.collection_ids.clone();
        collections.sort();
        held_collections.sort();
        let mut attachments = item.attachments.clone();
        let mut held_attachments = held.attachments.clone();
        attachments.sort_by(|a, b| a.id.cmp(&b.id));
        held_attachments.sort_by(|a, b| a.id.cmp(&b.id));
        if plan::marker_of(held).is_some()
            || comparable(held, None, &[], mode) != comparable(item, None, &[], mode)
            || held.folder_id != item.folder_id
            || held_collections != collections
            || held.in_trash
            || held.archived != item.archived
            || held_attachments != attachments
        {
            bail!(
                "source item {} changed during marker cleanup; mirroring must remain paused for reconciliation",
                item.id
            );
        }
        for attachment in &held.attachments {
            let data = source
                .account
                .download_attachment(&source.scope, held, attachment)
                .await?;
            let hash: [u8; 32] = Sha256::digest(&data).into();
            if hashes.get(&attachment.id) != Some(&hash) {
                bail!(
                    "attachment {} of source item {} changed during marker cleanup; mirroring must remain paused for reconciliation",
                    attachment.id,
                    item.id
                );
            }
        }
    }
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
    verified_source_ids: BTreeSet<String>,
    cleanup_source_items: Vec<Item>,
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
        let (kind, settings) = match mode {
            Mode::Personal => ("folder", ["INCLUDE_FOLDERS", "EXCLUDE_FOLDERS"]),
            Mode::Organization => ("collection", ["INCLUDE_COLLECTIONS", "EXCLUDE_COLLECTIONS"]),
        };
        // A misspelt name would otherwise select nothing without a word. The
        // names are vault content, so the warning names only the setting.
        let unmatched = [&filter.include, &filter.exclude].map(|patterns| {
            (patterns.iter())
                .filter(|pattern| !names.iter().any(|name| Filter::matches(pattern, name)))
                .count()
        });
        let before = source.items.iter().filter(|item| !item.in_trash).count();
        filter.apply(source, mode);
        let after = source.items.iter().filter(|item| !item.in_trash).count();
        for (setting, unmatched) in settings.into_iter().zip(unmatched) {
            if unmatched > 0 {
                self.warn(format!(
                    "{} in {setting} {} no {kind} in the source. Check the spelling.",
                    count(unmatched, "name"),
                    if unmatched == 1 { "matches" } else { "match" },
                ));
            }
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
        let dest_ids: HashSet<&str> = dest_vault
            .items
            .iter()
            .filter(|item| !item.in_trash)
            .map(|item| item.id.as_str())
            .collect();
        let unfinished = source_vault
            .items
            .iter()
            .filter(|item| {
                !self.verified_source_ids.contains(&item.id)
                    && plan::marker_of(item).is_some_and(|marker| {
                        marker.files_pending() && dest_ids.contains(marker.source_id.as_str())
                    })
            })
            .count();
        if unfinished > 0 {
            let n = unfinished;
            bail!(
                "{} in the source {} halfway through a pull, and this run would replace the \
                 changes in {} copies with {}. Run with --pull to finish the pull",
                count(n, "item"),
                if n == 1 { "is" } else { "are" },
                if n == 1 { "its" } else { "their" },
                if n == 1 { "it" } else { "them" },
            );
        }
        self.cleanup_source_items = source_vault
            .items
            .iter()
            .filter(|item| self.verified_source_ids.contains(&item.id) && !item.in_trash)
            .cloned()
            .collect();
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
        let source_traits = self
            .source
            .account
            .server_traits()
            .await
            .context("reading the source server's settings")?;
        self.leave_out(
            &mut source_vault,
            |item| traits.hides(item),
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
        check_source_shows(&plan, &dest_vault, &source_traits)?;
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
        let mut transfers: Vec<(usize, &Item, Vec<String>)> = Vec::new();
        for (entry_index, item) in &created {
            if pending(*entry_index) {
                transfers.push((*entry_index, item, Vec::new()));
            }
        }
        for (index, item) in &updated {
            let update = &plan.updates[*index];
            if update.sync_attachments {
                let old_attachment_ids = dest_vault.items[update.dest]
                    .attachments
                    .iter()
                    .map(|a| a.id.clone())
                    .collect();
                transfers.push((update.entry, item, old_attachment_ids));
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
        let copied: Vec<(usize, usize, Item)> = stream::iter(transfers.into_iter().map(
            |(entry_index, dest_item, old_attachment_ids)| {
                let (folder_ids, collection_ids, uploads) =
                    (&folder_ids, &collection_ids, &uploads);
                async move {
                    let entry = &plan.entries[entry_index];
                    let source_item = &source_vault.items[entry.source];
                    let (stored, copied) = copy_attachments(
                        (source, source_item),
                        (dest, dest_item),
                        &old_attachment_ids,
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
            },
        ))
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
        // no item.
        self.stage = "deleting obsolete items";
        if !plan.deletes.is_empty() {
            // The plan may come from the backup at the start of the run. An
            // item that changed in any way since holds what no backup does.
            // The request names items by id only, so an edit between this
            // read and it can still be lost; the trash keeps a way back.
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
    use crate::vault::{Attachment, Collection, Folder};

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
                changed_in_dest: false,
            }],
            ..Plan::default()
        };
        let traits = |hidden_item_types| ServerTraits {
            hidden_item_types,
            items_need_collections: false,
        };
        let refused = check_source_shows(&plan(DeleteReason::Stale), &dest, &traits(vec![6, 7, 8]));
        assert!(format!("{:#}", refused.unwrap_err()).contains("(copy)"));
        assert!(check_source_shows(&plan(DeleteReason::Stale), &dest, &traits(vec![])).is_ok());
        let duplicate = plan(DeleteReason::Duplicate);
        assert!(check_source_shows(&duplicate, &dest, &traits(vec![6, 7, 8])).is_ok());
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
