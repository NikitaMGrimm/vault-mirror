//! Decides what has to change in the destination. Pure: no I/O, no clock.
//!
//! Every copy carries a hidden custom field named `vault-mirror`, the
//! marker. It names the source item and records the state vault-mirror left
//! both items in, so a run needs nothing but the two vaults to tell which
//! side changed since.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    node::Node,
    text::count,
    vault::{Collection, Draft, Folder, Item, Vault},
};

pub const MARKER_FIELD: &str = "vault-mirror";
/// Custom field type "hidden".
const MARKER_FIELD_TYPE: u64 = 1;
/// Files hash of an item without attachments.
const NO_FILES: &str = "-";
/// Files hash before the attachments are copied and checked, so that any
/// comparison with it fails and the next run copies them again.
const UNKNOWN: &str = "?";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub source_id: String,
    /// The item's content when vault-mirror last wrote it. Content compares
    /// across servers, so this is the state of both sides at that moment.
    pub content: String,
    /// The source item's attachments when they were last copied.
    pub source_files: String,
    /// The copy's attachments as vault-mirror left them. Attachment ids
    /// differ between the two items, so each side gets its own hash.
    pub dest_files: String,
}

impl Marker {
    /// Accepts only what this tool writes: an item id and three hashes.
    /// Anything else is a field someone else named `vault-mirror`, and must
    /// not make an item look like a copy.
    fn parse(value: &str) -> Option<Self> {
        let is_hash = |part: &str| {
            part.len() == 64 && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let is_files = |part: &str| part == NO_FILES || part == UNKNOWN || is_hash(part);
        let mut parts = value.split_whitespace();
        let marker = Self {
            source_id: parts.next()?.to_string(),
            content: parts.next()?.to_string(),
            source_files: parts.next()?.to_string(),
            dest_files: parts.next()?.to_string(),
        };
        let valid = parts.next().is_none()
            && is_hash(&marker.content)
            && is_files(&marker.source_files)
            && is_files(&marker.dest_files);
        valid.then_some(marker)
    }

    fn render(&self) -> String {
        format!(
            "{} {} {} {}",
            self.source_id, self.content, self.source_files, self.dest_files
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
}

fn is_marker_field(field: &Node) -> bool {
    field
        .as_map()
        .and_then(|map| map.get("name"))
        .and_then(Node::as_str)
        == Some(MARKER_FIELD)
}

/// True for a `vault-mirror` field this tool wrote, as opposed to a field
/// someone else gave that name.
pub fn is_mirror_marker(field: &Node) -> bool {
    is_marker_field(field)
        && field
            .as_map()
            .and_then(|map| map.get("value"))
            .and_then(Node::as_str)
            .and_then(Marker::parse)
            .is_some()
}

/// The marker on an item. A malformed marker counts as none, so the item is
/// treated as not a copy rather than trusted.
pub fn marker_of(item: &Item) -> Option<Marker> {
    let field = item.fields().iter().rfind(|field| is_marker_field(field))?;
    Marker::parse(field.as_map()?.get("value")?.as_str()?)
}

/// A live source item with everything the comparison needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Index into the source vault's items.
    pub source: usize,
    pub folder: Option<String>,
    pub collections: Vec<String>,
    pub marker: Marker,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// Index into [`Plan::entries`].
    pub entry: usize,
    /// Index into the destination vault's items.
    pub dest: usize,
    pub write_content: bool,
    /// The copy was changed in the destination since vault-mirror last
    /// wrote it, and the source version replaces that change.
    pub changed_in_dest: bool,
    pub sync_attachments: bool,
    pub set_archived: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReason {
    /// Its source item no longer exists.
    Stale,
    /// A second copy of a source item that is already mirrored.
    Duplicate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub dest: usize,
    pub reason: DeleteReason,
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
    pub folders_to_delete: Vec<Folder>,
    pub collections_to_create: Vec<NewCollection>,
    pub collections_to_delete: Vec<Collection>,
    /// The destination folder that copies go into, by name. A run adds the
    /// folders it creates.
    pub folder_ids: HashMap<String, String>,
    /// The same for collections.
    pub collection_ids: HashMap<String, String>,
    /// Destination items that carry a marker, before this plan is applied.
    pub tracked_before: usize,
    /// Destination items without a marker. vault-mirror did not make them,
    /// so it leaves them, and the folders and collections they are in, alone.
    pub untracked: usize,
    /// Copies that a source item's own marker names: the source used to be
    /// the destination. Paired for the first time in this plan.
    pub reversed: usize,
    /// Items with the marker of a source item that already has a copy, and
    /// changed since vault-mirror wrote them, such as a cloned copy. Left
    /// alone.
    pub detached: usize,
    /// False when the destination cannot take attachments. Its existing
    /// attachments are then left as they are.
    pub copy_attachments: bool,
    /// Source attachments the destination lacks and does not get, because it
    /// cannot take attachments.
    pub attachments_withheld: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub items_created: usize,
    pub items_updated: usize,
    pub items_deleted: usize,
    pub items_unchanged: usize,
    pub folders_created: usize,
    pub folders_deleted: usize,
    pub collections_created: usize,
    pub collections_deleted: usize,
    pub attachments_copied: usize,
}

impl Plan {
    pub fn has_changes(&self) -> bool {
        !(self.creates.is_empty()
            && self.updates.is_empty()
            && self.deletes.is_empty()
            && self.folders_to_create.is_empty()
            && self.folders_to_delete.is_empty()
            && self.collections_to_create.is_empty()
            && self.collections_to_delete.is_empty())
    }

    pub fn summary(&self, source: &Vault) -> Summary {
        let attachments = |entry: usize| match self.copy_attachments {
            true => source.items[self.entries[entry].source].attachments.len(),
            false => 0,
        };
        Summary {
            items_created: self.creates.len(),
            items_updated: self.updates.len(),
            items_deleted: self.deletes.len(),
            items_unchanged: self.unchanged,
            folders_created: self.folders_to_create.len(),
            folders_deleted: self.folders_to_delete.len(),
            collections_created: self.collections_to_create.len(),
            collections_deleted: self.collections_to_delete.len(),
            attachments_copied: self
                .creates
                .iter()
                .map(|entry| attachments(*entry))
                .sum::<usize>()
                + self
                    .updates
                    .iter()
                    .filter(|update| update.sync_attachments)
                    .map(|update| attachments(update.entry))
                    .sum::<usize>(),
        }
    }
}

impl Entry {
    /// What to write to the destination for this entry.
    pub fn draft(
        &self,
        source: &Vault,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        self.draft_marked(&self.marker, source, folder_ids, collection_ids)
    }

    /// Like `draft`, but the marker does not yet vouch for the attachments.
    /// Written before attachments are copied, so that a run interrupted
    /// before they are complete and verified copies them again next time.
    pub fn pending_draft(
        &self,
        source: &Vault,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        let pending = Marker {
            source_files: UNKNOWN.to_string(),
            dest_files: UNKNOWN.to_string(),
            ..self.marker.clone()
        };
        self.draft_marked(&pending, source, folder_ids, collection_ids)
    }

    /// The draft that completes an item once its attachments are copied to
    /// `dest`, the destination item as the server now holds it.
    pub fn finished_draft(
        &self,
        source: &Vault,
        dest: &Item,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        let finished = Marker {
            source_files: source_files(&source.items[self.source]),
            dest_files: dest_files(dest),
            ..self.marker.clone()
        };
        self.draft_marked(&finished, source, folder_ids, collection_ids)
    }

    fn draft_marked(
        &self,
        marker: &Marker,
        source: &Vault,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        let item = &source.items[self.source];
        let mut content = item.content.clone();
        let mut fields: Vec<Node> = item
            .fields()
            .iter()
            .filter(|field| !is_marker_field(field))
            .cloned()
            .collect();
        fields.push(marker.to_field());
        content.insert("fields".to_string(), Node::List(fields));

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

pub fn plan(source: &Vault, dest: &Vault, mode: Mode) -> Plan {
    plan_with(source, dest, mode, true)
}

/// Plans a mirror. Without `copy_attachments`, attachments are neither
/// copied nor removed, and markers keep vouching for what the destination
/// already holds, so that the attachments follow once they can.
pub fn plan_with(source: &Vault, dest: &Vault, mode: Mode, copy_attachments: bool) -> Plan {
    let source_folders = names_by_id(source.folders.iter().map(|f| (&f.id, &f.name)));
    let source_collections = names_by_id(source.collections.iter().map(|c| (&c.id, &c.name)));
    // When the destination has two containers with one name, only the first
    // counts. Items in the others compare as misplaced and get moved.
    let dest_folders = first_id_by_name(dest.folders.iter().map(|f| (&f.id, &f.name)));
    let dest_collections = first_id_by_name(dest.collections.iter().map(|c| (&c.id, &c.name)));
    let dest_folder_names: HashMap<&str, &str> =
        dest_folders.iter().map(|(name, id)| (*id, *name)).collect();
    let dest_collection_names: HashMap<&str, &str> = dest_collections
        .iter()
        .map(|(name, id)| (*id, *name))
        .collect();

    let mut plan = Plan {
        copy_attachments,
        folder_ids: owned(&dest_folders),
        collection_ids: owned(&dest_collections),
        ..Plan::default()
    };

    let mut entry_by_source_id: HashMap<&str, usize> = HashMap::new();
    // A source item that is itself a copy names a destination item: the
    // two vaults have switched roles since that copy was made. Its marker
    // holds both items' state at the time.
    let mut reverse_links: HashMap<String, (usize, Marker)> = HashMap::new();
    for (index, item) in source.items.iter().enumerate() {
        if item.in_trash {
            continue;
        }
        let folder = match mode {
            Mode::Personal => item
                .folder_id
                .as_deref()
                .and_then(|id| source_folders.get(id))
                .map(|name| name.to_string()),
            Mode::Organization => None,
        };
        let collections = match mode {
            Mode::Personal => Vec::new(),
            Mode::Organization => sorted_names(
                item.collection_ids
                    .iter()
                    .filter_map(|id| source_collections.get(id.as_str()).copied()),
            ),
        };
        let marker = Marker {
            source_id: item.id.clone(),
            content: content_hash(item, folder.as_deref(), &collections, mode),
            source_files: source_files(item),
            // Final once the attachments are copied.
            dest_files: NO_FILES.to_string(),
        };
        if let Some(link) = marker_of(item) {
            reverse_links
                .entry(link.source_id.clone())
                .or_insert((plan.entries.len(), link));
        }
        entry_by_source_id.insert(&item.id, plan.entries.len());
        plan.entries.push(Entry {
            source: index,
            folder,
            collections,
            marker,
        });
    }

    let dest_content = |item: &Item| {
        let folder = match mode {
            Mode::Personal => item
                .folder_id
                .as_deref()
                .and_then(|id| dest_folder_names.get(id).copied()),
            Mode::Organization => None,
        };
        let collections = match mode {
            Mode::Personal => Vec::new(),
            Mode::Organization => sorted_names(
                item.collection_ids
                    .iter()
                    .filter_map(|id| dest_collection_names.get(id.as_str()).copied()),
            ),
        };
        content_hash(item, folder, &collections, mode)
    };
    // Unchanged since vault-mirror wrote it, as far as the marker tells.
    let pristine = |item: &Item, marker: &Marker| {
        dest_content(item) == marker.content
            && (marker.dest_files == UNKNOWN || marker.dest_files == dest_files(item))
    };

    // Each destination item that claims to be a copy, by the entry it copies,
    // with the marker that describes it and whether the link was reversed.
    let mut claims: BTreeMap<usize, Vec<(usize, Marker, bool)>> = BTreeMap::new();
    // Folders and collections that hold anything besides copies are kept.
    let mut untracked_folders: HashSet<&str> = dest
        .folders_used_elsewhere
        .iter()
        .map(String::as_str)
        .collect();
    let mut untracked_collections: HashSet<&str> = HashSet::new();
    let mut keep_containers = |index: usize| {
        let item = &dest.items[index];
        untracked_folders.extend(item.folder_id.as_deref());
        untracked_collections.extend(item.collection_ids.iter().map(String::as_str));
    };
    for (index, item) in dest.items.iter().enumerate() {
        let marker = marker_of(item);
        // The trash is left to the server, which empties it on its own
        // schedule. Until then whoever put an item there can take it back,
        // and backups do not cover it.
        if item.in_trash {
            if marker.is_none() {
                keep_containers(index);
            }
            continue;
        }
        let Some(marker) = marker else {
            match reverse_links.get(&item.id) {
                Some((entry, link)) => {
                    let entry = *entry;
                    let source_item = &source.items[plan.entries[entry].source];
                    // Neither item's attachments changed since the copy, so
                    // they still match and need no copying.
                    let files_in_sync = link.source_files == source_files(item)
                        && link.dest_files == dest_files(source_item);
                    let files = |hash: String| match files_in_sync {
                        true => hash,
                        false => UNKNOWN.to_string(),
                    };
                    let marker = Marker {
                        source_id: source_item.id.clone(),
                        content: link.content.clone(),
                        source_files: files(source_files(source_item)),
                        dest_files: files(dest_files(item)),
                    };
                    claims.entry(entry).or_default().push((index, marker, true));
                }
                None => {
                    plan.untracked += 1;
                    keep_containers(index);
                }
            }
            continue;
        };
        plan.tracked_before += 1;
        match entry_by_source_id.get(marker.source_id.as_str()) {
            Some(&entry) => claims
                .entry(entry)
                .or_default()
                .push((index, marker, false)),
            None => plan.deletes.push(Delete {
                dest: index,
                reason: DeleteReason::Stale,
            }),
        }
    }

    // One copy per source item: the first that is as vault-mirror left it,
    // else the first. Another unchanged one is a leftover and goes; a
    // changed one, such as a clone someone edited, is left alone.
    let mut dest_by_entry: HashMap<usize, (usize, Marker, bool)> = HashMap::new();
    for (entry, mut candidates) in claims {
        let chosen = candidates
            .iter()
            .position(|(index, marker, _)| pristine(&dest.items[*index], marker))
            .unwrap_or(0);
        let copy = candidates.remove(chosen);
        for (index, marker, _) in candidates {
            if pristine(&dest.items[index], &marker) {
                plan.deletes.push(Delete {
                    dest: index,
                    reason: DeleteReason::Duplicate,
                });
            } else {
                plan.detached += 1;
                keep_containers(index);
            }
        }
        dest_by_entry.insert(entry, copy);
    }
    plan.reversed = dest_by_entry
        .values()
        .filter(|(_, _, reversed)| *reversed)
        .count();

    for entry_index in 0..plan.entries.len() {
        let source_item = &source.items[plan.entries[entry_index].source];
        let Some((dest_index, marker, reversed)) = dest_by_entry.get(&entry_index) else {
            if !copy_attachments {
                plan.attachments_withheld += source_item.attachments.len();
            }
            plan.creates.push(entry_index);
            continue;
        };
        let dest_item = &dest.items[*dest_index];
        // The marker alone is not trusted: a destination item edited by hand
        // still carries the old marker.
        let dest_content = dest_content(dest_item);
        let files_changed_in_dest =
            marker.dest_files != UNKNOWN && marker.dest_files != dest_files(dest_item);
        let mut sync_attachments = marker.source_files
            != plan.entries[entry_index].marker.source_files
            || files_changed_in_dest
            || attachment_names(dest_item) != attachment_names(source_item);
        if !copy_attachments {
            // The marker goes on vouching for whatever the destination holds,
            // so that a later run that can copy attachments sees what is
            // missing.
            if sync_attachments {
                let held = attachment_names(dest_item);
                let missing = attachment_names(source_item)
                    .iter()
                    .filter(|name| !held.contains(name))
                    .count();
                // Files replaced under the same name are all missing.
                plan.attachments_withheld += match missing {
                    0 => source_item.attachments.len(),
                    missing => missing,
                };
            }
            sync_attachments = false;
            let entry = &mut plan.entries[entry_index];
            entry.marker.source_files = marker.source_files.clone();
            entry.marker.dest_files = marker.dest_files.clone();
        } else if !sync_attachments {
            plan.entries[entry_index].marker.dest_files = dest_files(dest_item);
        }
        let entry = &plan.entries[entry_index];
        // A reversed pair gets its first marker on this side.
        let write_content =
            *reversed || marker != &entry.marker || dest_content != entry.marker.content;
        // Only a change that the source version then overwrites counts.
        let changed_in_dest = (dest_content != marker.content
            && dest_content != entry.marker.content)
            || (copy_attachments && files_changed_in_dest);
        let set_archived =
            (dest_item.archived != source_item.archived).then_some(source_item.archived);

        if write_content || sync_attachments || set_archived.is_some() {
            plan.updates.push(Update {
                entry: entry_index,
                dest: *dest_index,
                write_content,
                changed_in_dest,
                sync_attachments,
                set_archived,
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
                .filter(|name| !dest_folders.contains_key(**name))
                .map(|name| name.to_string())
                .collect();
            plan.folders_to_delete = dest
                .folders
                .iter()
                .filter(|f| {
                    !untracked_folders.contains(f.id.as_str())
                        && (!wanted.contains(f.name.as_str())
                            || dest_folders.get(f.name.as_str()) != Some(&f.id.as_str()))
                })
                .cloned()
                .collect();
        }
        Mode::Organization => {
            let mut wanted: BTreeMap<&str, &Collection> = BTreeMap::new();
            for collection in &source.collections {
                wanted.entry(&collection.name).or_insert(collection);
            }
            plan.collections_to_create = wanted
                .iter()
                .filter(|(name, _)| !dest_collections.contains_key(**name))
                .map(|(name, collection)| NewCollection {
                    name: name.to_string(),
                    external_id: collection.external_id.clone(),
                })
                .collect();
            plan.collections_to_delete = dest
                .collections
                .iter()
                .filter(|c| {
                    !untracked_collections.contains(c.id.as_str())
                        && (!wanted.contains_key(c.name.as_str())
                            || dest_collections.get(c.name.as_str()) != Some(&c.id.as_str()))
                })
                .cloned()
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
) -> HashMap<&'a str, &'a str> {
    let mut map = HashMap::new();
    for (id, name) in pairs {
        map.entry(name.as_str()).or_insert(id.as_str());
    }
    map
}

fn owned(map: &HashMap<&str, &str>) -> HashMap<String, String> {
    map.iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn sorted_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let names: BTreeSet<&str> = names.collect();
    names.into_iter().map(str::to_string).collect()
}

fn attachment_names(item: &Item) -> Vec<&str> {
    let mut names: Vec<&str> = item
        .attachments
        .iter()
        .map(|a| a.file_name.as_str())
        .collect();
    names.sort_unstable();
    names
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
        fields.retain(|field| !is_marker_field(field));
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

/// Identifies a copy's attachments, by id for the same reason.
fn dest_files(item: &Item) -> String {
    if item.attachments.is_empty() {
        return NO_FILES.to_string();
    }
    let mut ids: Vec<&str> = item.attachments.iter().map(|a| a.id.as_str()).collect();
    ids.sort_unstable();
    hash(&json!(ids))
}

fn hash(value: &Value) -> String {
    // serde_json keeps insertion order and every map here is built sorted, so
    // equal values serialize identically.
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

/// Limits that stop a misconfigured run before it destroys a vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Safety {
    /// Refuse to delete more than this share of the copies, if
    /// set. Off by default: any fixed share is arbitrary, and the backup
    /// taken before the run holds what it deletes.
    pub max_delete_percent: Option<u32>,
    /// Below this many deletions the percentage is not checked, so small
    /// vaults can change freely.
    pub delete_floor: usize,
    /// Permit this run to delete every earlier copy, or more than
    /// `max_delete_percent`.
    pub allow_mass_delete: bool,
    /// Permit this run to pair copies the other way round, after the source
    /// and destination switched roles.
    pub allow_reverse: bool,
}

impl Default for Safety {
    fn default() -> Self {
        Self {
            max_delete_percent: None,
            delete_floor: 10,
            allow_mass_delete: false,
            allow_reverse: false,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SafetyViolation {
    #[error(
        "{} in the source {} copies of destination items, so the source and destination look \
         swapped. If you are switching them on purpose, run once with --reverse",
        count(*.0, "item"),
        if *.0 == 1 { "is" } else { "are" }
    )]
    Reversed(usize),
    #[error(
        "the run would delete every item it mirrored before ({0}) and keep none, so the source \
         looks empty or like a different vault. Pass --allow-mass-delete if this is expected"
    )]
    ReplacesEverything(usize),
    #[error(
        "the run would delete {deleting} of {total} copies ({percent}%), above the \
         {limit}% limit. Pass --allow-mass-delete if this is expected"
    )]
    MassDelete {
        deleting: usize,
        total: usize,
        percent: u32,
        limit: u32,
    },
}

pub fn check_safety(plan: &Plan, safety: &Safety) -> Result<(), SafetyViolation> {
    if plan.reversed > 0 && !safety.allow_reverse {
        return Err(SafetyViolation::Reversed(plan.reversed));
    }
    let stale = plan
        .deletes
        .iter()
        .filter(|delete| delete.reason == DeleteReason::Stale)
        .count();
    if safety.allow_mass_delete {
        return Ok(());
    }
    // Not one earlier copy still has its source item: the source read as
    // empty, or is another vault. A source that changes, however much, keeps
    // at least some of its items.
    if plan.tracked_before > 0 && stale == plan.tracked_before {
        return Err(SafetyViolation::ReplacesEverything(stale));
    }
    let (deleting, total) = (stale, plan.tracked_before);
    if let Some(limit) = safety.max_delete_percent
        && total > 0
        && deleting > safety.delete_floor
        // Compared exactly: 101 of 200 is above a 50% limit.
        && deleting as u128 * 100 > limit as u128 * total as u128
    {
        let percent = u32::try_from(deleting * 100 / total).unwrap_or(u32::MAX);
        return Err(SafetyViolation::MassDelete {
            deleting,
            total,
            percent,
            limit,
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
