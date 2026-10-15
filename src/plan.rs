//! Decides what has to change in the destination. Pure: no I/O, no clock.
//!
//! Every mirrored destination item carries a hidden custom field named
//! `__source` holding the id of the source item it mirrors and hashes of the
//! source content. That marker is the only state the mirror keeps, so a run
//! needs nothing but the two vaults to work out the difference.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    node::Node,
    text::count,
    vault::{Collection, Draft, Folder, Item, Vault},
};

pub const MARKER_FIELD: &str = "__source";
/// Custom field type "hidden".
const MARKER_FIELD_TYPE: u64 = 1;
/// Marker value for "the source item has no attachments".
const NO_ATTACHMENTS: &str = "-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub source_id: String,
    pub core_hash: String,
    pub attachments_hash: String,
}

impl Marker {
    /// Accepts only what this tool writes: an item id and two hex SHA-256
    /// hashes, the second of which may be [`NO_ATTACHMENTS`]. Anything else is
    /// a field someone else named `__source`, and must not make an item look
    /// mirrored.
    fn parse(value: &str) -> Option<Self> {
        let is_hash = |part: &str| {
            part.len() == 64 && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let mut parts = value.split_whitespace();
        let marker = Self {
            source_id: parts.next()?.to_string(),
            core_hash: parts.next()?.to_string(),
            attachments_hash: parts.next()?.to_string(),
        };
        let valid = parts.next().is_none()
            && is_hash(&marker.core_hash)
            && (marker.attachments_hash == NO_ATTACHMENTS || is_hash(&marker.attachments_hash));
        valid.then_some(marker)
    }

    fn render(&self) -> String {
        format!(
            "{} {} {}",
            self.source_id, self.core_hash, self.attachments_hash
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

/// How many live source items carry a `__source` field, valid or not. Any
/// such field means the source looks like a mirror, or holds a field the
/// mirror could not copy faithfully.
pub fn source_marker_count(source: &Vault) -> usize {
    source
        .items
        .iter()
        .filter(|item| !item.in_trash && item.fields().iter().any(is_marker_field))
        .count()
}

/// True for a `__source` field this tool wrote, as opposed to a field
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

/// The marker on a destination item. A malformed marker counts as none, so
/// the item is treated as not mirrored rather than trusted.
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
    /// Destination items that carry a marker, before this plan is applied.
    pub tracked_before: usize,
    /// Destination items without a marker. vault-mirror did not make them,
    /// so it leaves them, and the folders and collections they are in, alone.
    pub untracked: usize,
    /// Source items that carry a marker themselves.
    pub source_markers: usize,
    /// False when the destination cannot take attachments. Its existing
    /// attachments are then left as they are.
    pub copy_attachments: bool,
    /// Source attachments the destination lacks and does not get, because it
    /// cannot take attachments.
    pub attachments_withheld: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
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

    /// What to write to the destination for `entry`.
    pub fn draft(
        &self,
        entry: &Entry,
        source: &Vault,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        self.draft_marked(entry, &entry.marker, source, folder_ids, collection_ids)
    }

    /// Like `draft`, but the marker does not yet vouch for the attachments.
    /// Written before attachments are copied, so that a run interrupted
    /// before they are complete and verified copies them again next time.
    pub fn pending_draft(
        &self,
        entry: &Entry,
        source: &Vault,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        let pending = Marker {
            attachments_hash: NO_ATTACHMENTS.to_string(),
            ..entry.marker.clone()
        };
        self.draft_marked(entry, &pending, source, folder_ids, collection_ids)
    }

    /// The draft that completes an item once its attachments are copied to
    /// `dest`, the destination item as the server now holds it.
    pub fn finished_draft(
        &self,
        entry: &Entry,
        source: &Vault,
        dest: &Item,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        let finished = Marker {
            attachments_hash: attachments_hash(&source.items[entry.source], dest),
            ..entry.marker.clone()
        };
        self.draft_marked(entry, &finished, source, folder_ids, collection_ids)
    }

    fn draft_marked(
        &self,
        entry: &Entry,
        marker: &Marker,
        source: &Vault,
        folder_ids: &HashMap<String, String>,
        collection_ids: &HashMap<String, String>,
    ) -> Result<Draft, PlanError> {
        let item = &source.items[entry.source];
        let mut content = item.content.clone();
        let mut fields: Vec<Node> = item
            .fields()
            .iter()
            .filter(|field| !is_marker_field(field))
            .cloned()
            .collect();
        fields.push(marker.to_field());
        content.insert("fields".to_string(), Node::List(fields));

        let folder_id = match &entry.folder {
            Some(name) => Some(
                folder_ids
                    .get(name)
                    .cloned()
                    .ok_or_else(|| PlanError::MissingFolder(name.clone()))?,
            ),
            None => None,
        };
        let collection_ids = entry
            .collections
            .iter()
            .map(|name| {
                collection_ids
                    .get(name)
                    .cloned()
                    .ok_or_else(|| PlanError::MissingCollection(name.clone()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Draft {
            content,
            folder_id,
            collection_ids,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("the destination has no folder named {0:?}")]
    MissingFolder(String),
    #[error("the destination has no collection named {0:?}")]
    MissingCollection(String),
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
        ..Plan::default()
    };

    let mut entry_by_source_id: HashMap<&str, usize> = HashMap::new();
    for (index, item) in source.items.iter().enumerate() {
        if item.in_trash {
            continue;
        }
        if item.fields().iter().any(is_marker_field) {
            plan.source_markers += 1;
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
            core_hash: core_hash(item, folder.as_deref(), &collections, mode),
            // Final once the destination item is known.
            attachments_hash: NO_ATTACHMENTS.to_string(),
        };
        entry_by_source_id.insert(&item.id, plan.entries.len());
        plan.entries.push(Entry {
            source: index,
            folder,
            collections,
            marker,
        });
    }

    let mut dest_by_entry: HashMap<usize, (usize, Marker)> = HashMap::new();
    let mut untracked_folders: HashSet<&str> = HashSet::new();
    let mut untracked_collections: HashSet<&str> = HashSet::new();
    for (index, item) in dest.items.iter().enumerate() {
        let delete = |reason| Delete {
            dest: index,
            reason,
        };
        // The trash is left to the server, which empties it on its own
        // schedule. Until then whoever put an item there can take it back,
        // and backups do not cover it.
        if item.in_trash {
            continue;
        }
        let Some(marker) = marker_of(item) else {
            plan.untracked += 1;
            untracked_folders.extend(item.folder_id.as_deref());
            untracked_collections.extend(item.collection_ids.iter().map(String::as_str));
            continue;
        };
        plan.tracked_before += 1;
        let Some(&entry) = entry_by_source_id.get(marker.source_id.as_str()) else {
            plan.deletes.push(delete(DeleteReason::Stale));
            continue;
        };
        if dest_by_entry.contains_key(&entry) {
            plan.deletes.push(delete(DeleteReason::Duplicate));
            continue;
        }
        dest_by_entry.insert(entry, (index, marker));
    }

    for entry_index in 0..plan.entries.len() {
        let source_item = &source.items[plan.entries[entry_index].source];
        let Some((dest_index, marker)) = dest_by_entry.get(&entry_index) else {
            if !copy_attachments {
                plan.attachments_withheld += source_item.attachments.len();
            }
            plan.creates.push(entry_index);
            continue;
        };
        let dest_item = &dest.items[*dest_index];
        let dest_folder = match mode {
            Mode::Personal => dest_item
                .folder_id
                .as_deref()
                .and_then(|id| dest_folder_names.get(id).copied()),
            Mode::Organization => None,
        };
        let dest_item_collections = match mode {
            Mode::Personal => Vec::new(),
            Mode::Organization => sorted_names(
                dest_item
                    .collection_ids
                    .iter()
                    .filter_map(|id| dest_collection_names.get(id.as_str()).copied()),
            ),
        };
        // The marker alone is not trusted: a destination item edited by hand
        // still carries the old marker.
        let dest_core = core_hash(dest_item, dest_folder, &dest_item_collections, mode);
        let current = attachments_hash(source_item, dest_item);
        let mut sync_attachments = marker.attachments_hash != current
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
            plan.entries[entry_index].marker.attachments_hash = marker.attachments_hash.clone();
        } else if !sync_attachments {
            plan.entries[entry_index].marker.attachments_hash = current;
        }
        let entry = &plan.entries[entry_index];
        let write_content = marker != &entry.marker || dest_core != entry.marker.core_hash;
        // Only a change that the source version then overwrites counts.
        let changed_in_dest = dest_core != marker.core_hash && dest_core != entry.marker.core_hash;
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

fn core_hash(item: &Item, folder: Option<&str>, collections: &[String], mode: Mode) -> String {
    hash(&comparable(item, folder, collections, mode))
}

/// Identifies the source item's attachments and the destination files they
/// were copied to. Attachment ids never get reused, so a file replaced under
/// the same name on either side changes this.
fn attachments_hash(source: &Item, dest: &Item) -> String {
    if source.attachments.is_empty() && dest.attachments.is_empty() {
        return NO_ATTACHMENTS.to_string();
    }
    let mut identity: Vec<(&str, &str, u64)> = source
        .attachments
        .iter()
        .map(|a| (a.id.as_str(), a.file_name.as_str(), a.size))
        .collect();
    identity.sort_unstable();
    let mut copies: Vec<&str> = dest.attachments.iter().map(|a| a.id.as_str()).collect();
    copies.sort_unstable();
    hash(&json!([identity, copies]))
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
}

impl Default for Safety {
    fn default() -> Self {
        Self {
            max_delete_percent: None,
            delete_floor: 10,
            allow_mass_delete: false,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SafetyViolation {
    #[error(
        "the source has {} with a `__source` marker, so it looks like a mirror destination. \
         Check that the source and destination are not swapped",
        count(*.0, "item")
    )]
    SourceLooksLikeMirror(usize),
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
    if plan.source_markers > 0 {
        return Err(SafetyViolation::SourceLooksLikeMirror(plan.source_markers));
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
