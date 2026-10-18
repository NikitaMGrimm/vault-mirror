//! Decides what has to change in the destination. Pure: no I/O, no clock.
//!
//! Every copy carries a hidden custom field named `vault-mirror`, the
//! marker. It names the two vaults it was made between and the source item,
//! and records the state vault-mirror left the copy in, so a run needs
//! nothing but the two vaults to tell which side changed since.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    node::Node,
    text::{count, listed},
    vault::{Attachment, Draft, Item, Vault},
};

pub const MARKER_FIELD: &str = "vault-mirror";
/// Custom field type "hidden".
const MARKER_FIELD_TYPE: u64 = 1;
/// Files hash of an item without attachments.
const NO_FILES: &str = "-";
/// Files hash before the attachments are copied and checked. Nothing then
/// vouches for the attachments the copy holds.
const UNKNOWN: &str = "?";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The two vaults the copy was made between; see [`pair`].
    pub pair: String,
    pub source_id: String,
    /// The item's content when vault-mirror last wrote it. Content compares
    /// across servers, so this is the state of both sides at that moment.
    pub content: String,
    /// Whether the copy was archived when vault-mirror last wrote this.
    pub archived: bool,
    /// The source item's attachments when they were last copied.
    pub source_files: String,
    /// The copy's attachments as vault-mirror left them. Attachment ids
    /// differ between the two items, so each side gets its own hash.
    pub dest_files: String,
}

fn is_hash(part: &str) -> bool {
    part.len() == 64 && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Marker {
    /// Accepts only what this tool writes. Anything else is a field someone
    /// else named `vault-mirror`, and must not make an item look like a copy.
    fn parse(value: &str) -> Option<Self> {
        let is_files = |part: &str| part == NO_FILES || part == UNKNOWN || is_hash(part);
        let mut parts = value.split_whitespace();
        let marker = Self {
            pair: parts.next()?.to_string(),
            source_id: parts.next()?.to_string(),
            content: parts.next()?.to_string(),
            archived: match parts.next()? {
                "0" => false,
                "1" => true,
                _ => return None,
            },
            source_files: parts.next()?.to_string(),
            dest_files: parts.next()?.to_string(),
        };
        let valid = parts.next().is_none()
            && is_hash(&marker.pair)
            && is_hash(&marker.content)
            && is_files(&marker.source_files)
            && is_files(&marker.dest_files);
        valid.then_some(marker)
    }

    fn render(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.pair,
            self.source_id,
            self.content,
            u8::from(self.archived),
            self.source_files,
            self.dest_files
        )
    }

    fn to_field(&self) -> Node {
        Node::Map(BTreeMap::from([
            ("name".to_string(), Node::secret(MARKER_FIELD)),
            ("value".to_string(), Node::secret(self.render())),
            ("type".to_string(), Node::Number(MARKER_FIELD_TYPE.into())),
            ("linkedId".to_string(), Node::Null),
        ]))
    }

    fn files_pending(&self) -> bool {
        self.dest_files == UNKNOWN
    }
}

/// Names the two vaults of a mirror, each as its server's API address, the
/// account and the scope. The order does not matter, so copies keep their
/// owner when the source and destination switch roles.
pub fn pair(one: &str, other: &str) -> String {
    let mut vaults = [one, other];
    vaults.sort_unstable();
    hash(&json!(vaults))
}

/// Names the vaults of a mirror and which of them is the destination, for
/// the folders runs create there. Unlike [`pair`], the order matters: after
/// the vaults switch roles, the other one's folders are not this one's.
pub(crate) fn owner(pair: &str, dest: &str) -> String {
    hash(&json!([pair, dest]))
}

/// A personal folder a run created, with the stamp its name had then.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Receipt {
    pub folder_id: String,
    pub stamp: String,
}

#[derive(Debug, Default)]
pub(crate) struct FolderCleanup {
    /// Folders nothing needs any more, in folder id order.
    pub obsolete: Vec<Receipt>,
    /// Receipts for absent folders or folders whose encrypted-name stamps changed.
    pub stale: Vec<Receipt>,
}

/// Decides which folders runs created can go, from `made`. A folder can go
/// when its encrypted-name stamp still matches its receipt, no selected source folder
/// has that name, and no destination item is in it, not even in the trash or
/// in an organization. Given the `plan` for `source` and `dest`, a copy the
/// plan rewrites counts as in the folder the plan moves it to.
pub(crate) fn folder_cleanup(
    made: &BTreeSet<Receipt>,
    source: &Vault,
    dest: &Vault,
    plan: Option<&Plan>,
) -> FolderCleanup {
    let wanted: BTreeSet<&str> = source.folders.iter().map(|f| f.name.as_str()).collect();
    // The folder each copy the plan rewrites ends up in. A folder the plan
    // creates has no id yet, and is none that runs made before.
    let moves: HashMap<usize, Option<&str>> = (plan.into_iter())
        .flat_map(|plan| {
            (plan.updates.iter())
                .filter(|update| update.write_content || update.sync_attachments)
                .map(|update| {
                    let name = plan.entries[update.entry].folder.as_ref();
                    let id = name.and_then(|name| plan.folder_ids.get(name));
                    (update.dest, id.map(String::as_str))
                })
        })
        .collect();
    let mut cleanup = FolderCleanup::default();
    for receipt in made {
        let mut found = dest.folders.iter().filter(|f| f.id == receipt.folder_id);
        let folder = match (found.next(), found.next()) {
            (None, _) => {
                cleanup.stale.push(receipt.clone());
                continue;
            }
            (Some(folder), None) => folder,
            // Ambiguous: neither gone nor known to be empty.
            (Some(_), Some(_)) => continue,
        };
        match &folder.stamp {
            Some(stamp) if *stamp == receipt.stamp => {}
            Some(_) => {
                cleanup.stale.push(receipt.clone());
                continue;
            }
            None => continue,
        }
        let used = (dest.items.iter().enumerate()).any(|(index, item)| {
            let at = moves
                .get(&index)
                .copied()
                .unwrap_or(item.folder_id.as_deref());
            at == Some(folder.id.as_str())
        });
        if !wanted.contains(folder.name.as_str())
            && !used
            && !dest.folders_used_elsewhere.contains(&folder.id)
        {
            cleanup.obsolete.push(receipt.clone());
        }
    }
    cleanup
}

fn field_marker(field: &Node) -> Option<Marker> {
    let map = field.as_map()?;
    if map.get("name")?.as_str()? != MARKER_FIELD {
        return None;
    }
    Marker::parse(map.get("value")?.as_str()?)
}

/// True for a `vault-mirror` field this tool wrote, for any pair of vaults.
/// A field someone else gave that name is their content, and is copied like
/// any other.
pub fn is_mirror_marker(field: &Node) -> bool {
    field_marker(field).is_some()
}

/// The marker that makes `item` a copy made between the vaults of `pair`. A
/// marker of another pair, such as from mirroring the destination on into a
/// third vault, and a malformed marker count as none.
pub fn marker_of(item: &Item, pair: &str) -> Option<Marker> {
    item.fields()
        .iter()
        .rev()
        .filter_map(field_marker)
        .find(|marker| marker.pair == pair)
}

/// A live source item with everything the comparison needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Index into the source vault's items.
    pub source: usize,
    pub folder: Option<String>,
    pub collections: Vec<String>,
    /// What a copy that holds the source item and its files carries.
    pub marker: Marker,
}

/// What changed in a copy since vault-mirror last wrote it, and the run
/// would undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Changes {
    /// Its content, folder or collections.
    pub content: bool,
    pub archive: bool,
    /// Its attachments, or attachments nothing vouches for.
    pub files: bool,
}

impl Changes {
    pub fn any(&self) -> bool {
        self.content || self.archive || self.files
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// Index into [`Plan::entries`].
    pub entry: usize,
    /// Index into the destination vault's items.
    pub dest: usize,
    pub write_content: bool,
    pub sync_attachments: bool,
    pub set_archived: Option<bool>,
    pub changed_in_dest: Changes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReason {
    /// Its source item no longer exists.
    Stale,
    /// A second copy of a source item, holding nothing the kept copy lacks.
    Duplicate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub dest: usize,
    pub reason: DeleteReason,
    pub changed_in_dest: Changes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewCollection {
    pub name: String,
    pub external_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub entries: Vec<Entry>,
    /// Indexes into `entries`.
    pub creates: Vec<usize>,
    pub updates: Vec<Update>,
    pub deletes: Vec<Delete>,
    pub unchanged: usize,
    pub folders_to_create: Vec<String>,
    pub collections_to_create: Vec<NewCollection>,
    /// The destination folder that copies go into, by name. A run adds the
    /// folders it creates.
    pub folder_ids: HashMap<String, String>,
    /// The same for collections.
    pub collection_ids: HashMap<String, String>,
    /// Live copies with this pair's marker, before this plan is applied.
    pub tracked_before: usize,
    /// Destination items without this pair's marker. vault-mirror did not
    /// make them, so it leaves them alone.
    pub untracked: usize,
    /// Copies that a source item's own marker names: the source used to be
    /// the destination. Paired for the first time in this plan.
    pub reversed: usize,
    /// Items with the marker of a source item that already has a copy, that
    /// may hold something that copy lacks, such as a cloned copy. Left alone.
    pub detached: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub items_created: usize,
    pub items_updated: usize,
    pub items_deleted: usize,
    pub items_unchanged: usize,
    pub folders_created: usize,
    /// Folders a run made that nothing needs any more. A check reports how
    /// many a run would delete, a run how many it deleted.
    pub folders_deleted: usize,
    pub collections_created: usize,
    pub attachments_copied: usize,
}

impl Plan {
    pub fn has_changes(&self) -> bool {
        !(self.creates.is_empty()
            && self.updates.is_empty()
            && self.deletes.is_empty()
            && self.folders_to_create.is_empty()
            && self.collections_to_create.is_empty())
    }

    pub fn summary(&self, source: &Vault) -> Summary {
        let attachments = |entry: usize| source.items[self.entries[entry].source].attachments.len();
        Summary {
            items_created: self.creates.len(),
            items_updated: self.updates.len(),
            items_deleted: self.deletes.len(),
            items_unchanged: self.unchanged,
            folders_created: self.folders_to_create.len(),
            folders_deleted: 0,
            collections_created: self.collections_to_create.len(),
            attachments_copied: (self.creates.iter().copied())
                .chain(
                    (self.updates.iter())
                        .filter(|update| update.sync_attachments)
                        .map(|update| update.entry),
                )
                .map(attachments)
                .sum(),
        }
    }

    /// Destination items changed since vault-mirror wrote them that this
    /// plan replaces or deletes.
    pub fn overwritten(&self) -> impl Iterator<Item = usize> + '_ {
        (self.updates.iter())
            .filter(|update| update.changed_in_dest.any())
            .map(|update| update.dest)
            .chain(
                (self.deletes.iter())
                    .filter(|delete| delete.changed_in_dest.any())
                    .map(|delete| delete.dest),
            )
    }
}

impl Entry {
    /// What to write to the destination for this entry: the source item's
    /// content with a marker for a copy in the `archived` state. `files` are
    /// the copy's attachments, once they are the source item's. Without them
    /// nothing vouches for its attachments, so that a run interrupted before
    /// they are copied and checked does not take them for complete.
    pub fn draft(
        &self,
        source: &Vault,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
        archived: bool,
        files: Option<&[Attachment]>,
    ) -> Result<Draft, PlanError> {
        let source_item = &source.items[self.source];
        let (source_files, dest_files) = match files {
            Some(files) => (self.marker.source_files.clone(), dest_files(files)),
            None => (UNKNOWN.into(), UNKNOWN.into()),
        };
        let marker = Marker {
            archived,
            source_files,
            dest_files,
            ..self.marker.clone()
        };
        let mut content = unmarked(source_item);
        if let Some(Node::List(fields)) = content.get_mut("fields") {
            fields.push(marker.to_field());
        }

        let folder_id = match &self.folder {
            Some(name) => Some(
                folder_ids
                    .get(name)
                    .cloned()
                    .ok_or(PlanError::MissingFolder)?,
            ),
            None => None,
        };
        let collection_ids = self
            .collections
            .iter()
            .map(|name| {
                collection_ids
                    .get(name)
                    .cloned()
                    .ok_or(PlanError::MissingCollection)
            })
            .collect::<Result<_, _>>()?;
        Ok(Draft {
            content,
            folder_id,
            collection_ids,
        })
    }

    /// Whether `copy` carries the marker this entry would write for it now,
    /// holding the source item's files in the source item's archive state.
    pub fn marks(&self, copy: &Item) -> bool {
        let wanted = Marker {
            dest_files: dest_files(&copy.attachments),
            ..self.marker.clone()
        };
        marker_of(copy, &self.marker.pair).as_ref() == Some(&wanted)
    }
}

/// Folder and collection names are vault content, so these errors, which
/// reach the status file and notifications, don't name them.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("the destination is missing a folder that a copy belongs in")]
    MissingFolder,
    #[error("the destination is missing a collection that a copy belongs in")]
    MissingCollection,
}

/// Whether a run mirrors an individual vault or an organization vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Folders are mirrored by name; collections do not exist.
    Personal,
    /// Collections are mirrored by name; folders and favorites are per-user
    /// and ignored.
    Organization,
}

/// Names of a vault's folders and collections by id, and the item content
/// hashes they give.
struct Places<'a> {
    folders: HashMap<&'a str, &'a str>,
    collections: HashMap<&'a str, &'a str>,
    mode: Mode,
}

impl<'a> Places<'a> {
    fn new(vault: &'a Vault, mode: Mode) -> Self {
        Self {
            folders: names_by_id(vault.folders.iter().map(|f| (&f.id, &f.name))),
            collections: names_by_id(vault.collections.iter().map(|c| (&c.id, &c.name))),
            mode,
        }
    }

    fn folder(&self, item: &Item) -> Option<&'a str> {
        match self.mode {
            Mode::Personal => item
                .folder_id
                .as_deref()
                .and_then(|id| self.folders.get(id).copied()),
            Mode::Organization => None,
        }
    }

    fn collections(&self, item: &Item) -> Vec<String> {
        match self.mode {
            Mode::Personal => Vec::new(),
            Mode::Organization => sorted_names(
                item.collection_ids
                    .iter()
                    .filter_map(|id| self.collections.get(id.as_str()).copied()),
            ),
        }
    }

    fn content(&self, item: &Item) -> String {
        content_hash(item, self.folder(item), &self.collections(item), self.mode)
    }
}

/// What the markers tell about a destination item paired with a source item:
/// its own marker, or for an original only the source item's, which records
/// the same pair the other way round.
struct Known {
    /// The destination item's content when vault-mirror last wrote it.
    content: String,
    archived: bool,
    files: Files,
}

/// The destination item's attachments, as far as the markers tell.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Files {
    /// As vault-mirror left them, and with `current` the source item's.
    Unchanged {
        current: bool,
    },
    Changed,
    Unknown,
}

impl Known {
    fn new(own: Option<&Marker>, link: Option<&Marker>, source: &Item, dest: &Item) -> Self {
        let marker = own
            .or(link)
            .expect("a pair is linked by at least one marker");
        let files = if let Some(link) = link
            && link.source_files == source_files(dest)
            && link.dest_files == dest_files(&source.attachments)
        {
            // Both items' attachments are as the source item's marker
            // recorded them, however old the destination's own marker is.
            Files::Unchanged { current: true }
        } else {
            match (own, link) {
                (Some(own), _) if own.files_pending() => Files::Unknown,
                (Some(own), _) if own.dest_files == dest_files(&dest.attachments) => {
                    Files::Unchanged {
                        current: own.source_files == source_files(source),
                    }
                }
                (Some(_), _) => Files::Changed,
                // The original was the source side of that marker.
                (None, Some(link)) if link.source_files == UNKNOWN => Files::Unknown,
                (None, Some(link)) if link.source_files == source_files(dest) => {
                    Files::Unchanged { current: false }
                }
                (None, _) => Files::Changed,
            }
        };
        Self {
            content: marker.content.clone(),
            archived: marker.archived,
            files,
        }
    }
}

pub fn plan(source: &Vault, dest: &Vault, mode: Mode, pair: &str) -> Plan {
    let source_places = Places::new(source, mode);
    let dest_places = Places::new(dest, mode);
    // When the destination has two containers with one name, new copies go
    // into the first. Copies in either are where they belong.
    let mut plan = Plan {
        folder_ids: first_id_by_name(dest.folders.iter().map(|f| (&f.id, &f.name))),
        collection_ids: first_id_by_name(dest.collections.iter().map(|c| (&c.id, &c.name))),
        ..Plan::default()
    };

    let mut entry_by_source_id: HashMap<&str, usize> = HashMap::new();
    // A source item can itself be a copy, made when the two vaults had the
    // other roles. Its marker names a destination item. Indexed like
    // `plan.entries`.
    let mut links: Vec<(&Item, Option<Marker>)> = Vec::new();
    let mut entry_by_link: HashMap<String, usize> = HashMap::new();
    for (index, item) in source.items.iter().enumerate() {
        if item.in_trash {
            continue;
        }
        let folder = source_places.folder(item).map(str::to_string);
        let collections = source_places.collections(item);
        let marker = Marker {
            pair: pair.to_string(),
            source_id: item.id.clone(),
            content: content_hash(item, folder.as_deref(), &collections, mode),
            archived: item.archived,
            source_files: source_files(item),
            dest_files: NO_FILES.to_string(),
        };
        let link = marker_of(item, pair);
        if let Some(link) = &link {
            entry_by_link
                .entry(link.source_id.clone())
                .or_insert(plan.entries.len());
        }
        links.push((item, link));
        entry_by_source_id.insert(&item.id, plan.entries.len());
        plan.entries.push(Entry {
            source: index,
            folder,
            collections,
            marker,
        });
    }
    let known = |entry: usize, dest_index: usize, own: Option<&Marker>| {
        let dest_item = &dest.items[dest_index];
        let (source_item, link) = &links[entry];
        let link = link.as_ref().filter(|link| link.source_id == dest_item.id);
        Known::new(own, link, source_item, dest_item)
    };
    // Whether a copy is exactly as its own marker says vault-mirror left it.
    let as_left = |index: usize, own: Option<&Marker>| {
        let item = &dest.items[index];
        own.is_some_and(|own| {
            dest_places.content(item) == own.content
                && item.archived == own.archived
                && match own.files_pending() {
                    true => item.attachments.is_empty(),
                    false => own.dest_files == dest_files(&item.attachments),
                }
        })
    };

    // Each destination item that claims to be a copy, by the entry it copies,
    // with its own marker. An item that only the source item's marker names
    // has none: it is the original that the source item was copied from.
    let mut claims: BTreeMap<usize, Vec<(usize, Option<Marker>)>> = BTreeMap::new();
    for (index, item) in dest.items.iter().enumerate() {
        // The trash is left to the server, which empties it on its own
        // schedule. Until then whoever put an item there, a run included, can
        // take it back, and backups do not cover it.
        if item.in_trash {
            continue;
        }
        let marker = marker_of(item, pair);
        if marker.is_some() {
            plan.tracked_before += 1;
        }
        let entry = marker
            .as_ref()
            .and_then(|marker| entry_by_source_id.get(marker.source_id.as_str()));
        match (entry, entry_by_link.get(&item.id)) {
            (Some(&entry), _) => claims.entry(entry).or_default().push((index, marker)),
            // An original, or a copy whose source item is gone, that a source
            // item copied from it names.
            (None, Some(&entry)) => claims.entry(entry).or_default().push((index, None)),
            (None, None) => match &marker {
                Some(marker) => plan.deletes.push(Delete {
                    dest: index,
                    reason: DeleteReason::Stale,
                    changed_in_dest: Changes {
                        content: dest_places.content(item) != marker.content,
                        archive: item.archived != marker.archived,
                        // Its source item is gone, so nothing can prove
                        // attachments that no marker vouches for.
                        files: match marker.files_pending() {
                            true => !item.attachments.is_empty(),
                            false => marker.dest_files != dest_files(&item.attachments),
                        },
                    },
                }),
                None => plan.untracked += 1,
            },
        }
    }

    // One copy per source item: the first that is as vault-mirror left it,
    // else the first that seems so, else the first. Another copy is deleted
    // only if both are as vault-mirror left them, with complete markers that
    // record the same source content, attachments and archive state. Any
    // other, such as a clone someone edited, is left alone, and so is an
    // original.
    let mut dest_by_entry: HashMap<usize, (usize, Option<Marker>)> = HashMap::new();
    for (entry, mut candidates) in claims {
        let chosen = (candidates.iter())
            .position(|(index, own)| as_left(*index, own.as_ref()))
            .or_else(|| {
                candidates.iter().position(|(index, own)| {
                    let known = known(entry, *index, own.as_ref());
                    dest_places.content(&dest.items[*index]) == known.content
                        && known.files != Files::Changed
                })
            })
            .unwrap_or(0);
        let copy = candidates.remove(chosen);
        let complete = |index: usize, own: Option<&Marker>| {
            as_left(index, own) && own.is_some_and(|own| !own.files_pending())
        };
        let kept = copy.1.as_ref().filter(|kept| complete(copy.0, Some(kept)));
        for (index, own) in candidates {
            let redundant = complete(index, own.as_ref())
                && own.as_ref().zip(kept).is_some_and(|(own, kept)| {
                    (&own.content, own.archived, &own.source_files)
                        == (&kept.content, kept.archived, &kept.source_files)
                });
            if redundant {
                plan.deletes.push(Delete {
                    dest: index,
                    reason: DeleteReason::Duplicate,
                    changed_in_dest: Changes::default(),
                });
            } else if own.is_some() {
                plan.detached += 1;
            } else {
                plan.untracked += 1;
            }
        }
        dest_by_entry.insert(entry, copy);
    }
    plan.reversed = dest_by_entry
        .values()
        .filter(|(index, _)| marker_of(&dest.items[*index], pair).is_none())
        .count();

    for entry_index in 0..links.len() {
        let Some((dest_index, own)) = dest_by_entry.get(&entry_index) else {
            plan.creates.push(entry_index);
            continue;
        };
        let source_item = &source.items[plan.entries[entry_index].source];
        let dest_item = &dest.items[*dest_index];
        let known = known(entry_index, *dest_index, own.as_ref());
        let entry = &plan.entries[entry_index];
        // The marker alone is not trusted: a destination item edited by hand
        // still carries the old marker.
        let dest_content = dest_places.content(dest_item);
        let mut changed_in_dest = Changes {
            // A change that the source made too is lost by no one.
            content: dest_content != known.content && dest_content != entry.marker.content,
            archive: dest_item.archived != known.archived
                && dest_item.archived != source_item.archived,
            files: false,
        };
        let files = match known.files {
            // Copying stopped before the first file arrived: nothing to lose.
            Files::Unknown if dest_item.attachments.is_empty() => Files::Unchanged {
                current: source_item.attachments.is_empty(),
            },
            files => files,
        };
        changed_in_dest.files = matches!(files, Files::Changed | Files::Unknown);
        let sync_attachments = files != Files::Unchanged { current: true };
        let write_content = !entry.marks(dest_item) || dest_content != entry.marker.content;
        let set_archived =
            (dest_item.archived != source_item.archived).then_some(source_item.archived);

        if write_content || sync_attachments || set_archived.is_some() {
            plan.updates.push(Update {
                entry: entry_index,
                dest: *dest_index,
                write_content,
                sync_attachments,
                set_archived,
                changed_in_dest,
            });
        } else {
            plan.unchanged += 1;
        }
    }

    match mode {
        Mode::Personal => {
            let wanted: BTreeSet<&str> = source.folders.iter().map(|f| f.name.as_str()).collect();
            plan.folders_to_create = wanted
                .iter()
                .filter(|name| !plan.folder_ids.contains_key(**name))
                .map(|name| name.to_string())
                .collect();
        }
        Mode::Organization => {
            let mut wanted: BTreeMap<&str, &crate::vault::Collection> = BTreeMap::new();
            for collection in &source.collections {
                wanted.entry(&collection.name).or_insert(collection);
            }
            plan.collections_to_create = wanted
                .iter()
                .filter(|(name, _)| !plan.collection_ids.contains_key(**name))
                .map(|(name, collection)| NewCollection {
                    name: name.to_string(),
                    external_id: collection.external_id.clone(),
                })
                .collect();
        }
    }
    plan
}

fn names_by_id<'a>(
    pairs: impl Iterator<Item = (&'a String, &'a String)>,
) -> HashMap<&'a str, &'a str> {
    pairs
        .map(|(id, name)| (id.as_str(), name.as_str()))
        .collect()
}

fn first_id_by_name<'a>(
    pairs: impl Iterator<Item = (&'a String, &'a String)>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (id, name) in pairs {
        map.entry(name.clone()).or_insert(id.clone());
    }
    map
}

fn sorted_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let names: BTreeSet<&str> = names.collect();
    names.into_iter().map(str::to_string).collect()
}

/// The item's content in a form that is equal for the same item on two servers.
pub fn comparable(item: &Item, folder: Option<&str>, collections: &[String], mode: Mode) -> Value {
    let mut content = item.content.clone();
    if mode == Mode::Organization {
        // Bitwarden's server keeps an organization item's favorite flag per
        // member and ignores it when the item is written.
        content.remove("favorite");
    }
    if let Some(Node::List(fields)) = content.get_mut("fields") {
        fields.retain(|field| !is_mirror_marker(field));
    }
    if let Some(Node::Map(login)) = content.get_mut("login") {
        // Servers echo the first URI a second time as `uri` for old clients.
        let has_uris = login
            .get("uris")
            .and_then(Node::as_list)
            .is_some_and(|uris| !uris.is_empty());
        if has_uris {
            login.remove("uri");
        }
    }
    json!({
        "content": Node::Map(content).normalized(),
        "folder": folder,
        "collections": collections,
    })
}

/// `item`'s content without the markers vault-mirror wrote.
pub fn unmarked(item: &Item) -> BTreeMap<String, Node> {
    let mut content = item.content.clone();
    let fields = (item.fields().iter())
        .filter(|field| !is_mirror_marker(field))
        .cloned()
        .collect();
    content.insert("fields".to_string(), Node::List(fields));
    content
}

fn content_hash(item: &Item, folder: Option<&str>, collections: &[String], mode: Mode) -> String {
    hash(&comparable(item, folder, collections, mode))
}

/// Identifies a source item's attachments. Attachment ids never get reused,
/// so a file replaced under the same name changes this.
fn source_files(item: &Item) -> String {
    if item.attachments.is_empty() {
        return NO_FILES.to_string();
    }
    let mut files: Vec<(&str, &str, u64)> = item
        .attachments
        .iter()
        .map(|a| (a.id.as_str(), a.file_name.as_str(), a.size))
        .collect();
    files.sort_unstable();
    hash(&json!(files))
}

/// Identifies a copy's attachments. File content is immutable under an id,
/// but its name can change without replacing the attachment.
fn dest_files(files: &[Attachment]) -> String {
    if files.is_empty() {
        return NO_FILES.to_string();
    }
    let mut files: Vec<(&str, &str)> = files
        .iter()
        .map(|a| (a.id.as_str(), a.file_name.as_str()))
        .collect();
    files.sort_unstable();
    hash(&json!(files))
}

fn hash(value: &Value) -> String {
    // serde_json keeps insertion order and every map here is built sorted, so
    // equal values serialize identically.
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

/// Limits that stop a misconfigured run before it destroys data. Each one
/// applies to one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Safety {
    /// Permit this run to delete more than half of the copies.
    pub allow_mass_delete: bool,
    /// Permit this run to pair copies the other way round, after the source
    /// and destination switched roles.
    pub allow_reverse: bool,
    /// Permit this run to replace or delete copies changed in the
    /// destination. Their changes are in the destination backup.
    pub allow_overwrite: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SafetyViolation {
    #[error(
        "the source holds {} copied from destination items, so the source and destination \
         look swapped. If you are switching them on purpose, run once with --reverse",
        count(*.0, "item")
    )]
    Reversed(usize),
    #[error(
        "{} changed in the destination since vault-mirror wrote {them}, and the run would \
         replace or delete {them} ({ids}). Make lasting changes in the source. To replace \
         {them} with the source version, run once with --allow-overwrite; the destination \
         backup keeps the changes",
        count(.0.len(), "copy"),
        them = if .0.len() == 1 { "it" } else { "them" },
        ids = listed(.0),
    )]
    Overwrites(Vec<String>),
    #[error(
        "the run would delete {deleted} of the {total} copies, more than half, so the source \
         looks empty, filtered too much or like a different vault. Pass --allow-mass-delete \
         if this is expected"
    )]
    MassDelete { deleted: usize, total: usize },
}

pub fn check_safety(plan: &Plan, dest: &Vault, safety: &Safety) -> Result<(), SafetyViolation> {
    if plan.reversed > 0 && !safety.allow_reverse {
        return Err(SafetyViolation::Reversed(plan.reversed));
    }
    let overwritten: BTreeSet<&str> = (plan.overwritten())
        .map(|index| dest.items[index].id.as_str())
        .collect();
    if !overwritten.is_empty() && !safety.allow_overwrite {
        return Err(SafetyViolation::Overwrites(
            overwritten.into_iter().map(str::to_string).collect(),
        ));
    }
    // A source that changes, however much, keeps most of its items. One that
    // reads as empty or is another vault keeps few or none.
    let deleted = plan.deletes.len();
    if deleted * 2 > plan.tracked_before && !safety.allow_mass_delete {
        return Err(SafetyViolation::MassDelete {
            deleted,
            total: plan.tracked_before,
        });
    }
    Ok(())
}

/// Lists where two comparable values differ, as paths without any values, so
/// the result is safe to log.
pub fn differing_paths(left: &Value, right: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    diff(left, right, String::new(), &mut paths);
    paths
}

fn diff(left: &Value, right: &Value, path: String, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            let keys: BTreeSet<&String> = l.keys().chain(r.keys()).collect();
            for key in keys {
                let next = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                diff(
                    l.get(key).unwrap_or(&Value::Null),
                    r.get(key).unwrap_or(&Value::Null),
                    next,
                    out,
                );
            }
        }
        (Value::Array(l), Value::Array(r)) if l.len() == r.len() => {
            for (index, (l, r)) in l.iter().zip(r).enumerate() {
                diff(l, r, format!("{path}[{index}]"), out);
            }
        }
        (l, r) if l == r => {}
        _ => out.push(if path.is_empty() {
            "<root>".to_string()
        } else {
            path
        }),
    }
}

#[cfg(test)]
mod tests;
