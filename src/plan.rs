//! Decides what has to change in the destination. Pure: no I/O, no clock.
//!
//! Every mirrored destination item carries a hidden custom field named
//! `__source` holding the id of the source item it mirrors and hashes of the
//! source content. That marker is the only state the mirror keeps, so a run
//! needs nothing but the two vaults to work out the difference.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    node::Node,
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
    /// `None` for markers written by the original Python tool, which stored a
    /// single combined hash.
    pub attachments_hash: Option<String>,
}

impl Marker {
    fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split_whitespace();
        let marker = Self {
            source_id: parts.next()?.to_string(),
            core_hash: parts.next()?.to_string(),
            attachments_hash: parts.next().map(str::to_string),
        };
        parts.next().is_none().then_some(marker)
    }

    fn render(&self) -> String {
        match &self.attachments_hash {
            Some(attachments) => format!("{} {} {attachments}", self.source_id, self.core_hash),
            None => format!("{} {}", self.source_id, self.core_hash),
        }
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
    pub sync_attachments: bool,
    pub set_archived: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReason {
    /// Its source item no longer exists.
    Stale,
    /// It was never mirrored: someone added it to the destination.
    Untracked,
    /// A second copy of a source item that is already mirrored.
    Duplicate,
    /// It sits in the destination's trash.
    Trashed,
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
    /// Source items that carry a marker themselves.
    pub source_markers: usize,
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
        let attachments = |entry: usize| source.items[self.entries[entry].source].attachments.len();
        Summary {
            items_created: self.creates.len(),
            items_updated: self.updates.len(),
            items_deleted: self.deletes.len(),
            items_unchanged: self.unchanged,
            folders_created: self.folders_to_create.len(),
            folders_deleted: self.folders_to_delete.len(),
            collections_created: self.collections_to_create.len(),
            collections_deleted: self.collections_to_delete.len(),
            attachments_copied: self.creates.iter().map(|entry| attachments(*entry)).sum::<usize>()
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
        let item = &source.items[entry.source];
        let mut content = item.content.clone();
        let mut fields: Vec<Node> = item
            .fields()
            .iter()
            .filter(|field| !is_marker_field(field))
            .cloned()
            .collect();
        fields.push(entry.marker.to_field());
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
    /// Collections are mirrored by name; folders are per-user and ignored.
    Organization,
}

pub fn plan(source: &Vault, dest: &Vault, mode: Mode) -> Plan {
    let source_folders = names_by_id(source.folders.iter().map(|f| (&f.id, &f.name)));
    let source_collections = names_by_id(source.collections.iter().map(|c| (&c.id, &c.name)));
    // When the destination has two containers with one name, only the first
    // counts. Items in the others compare as misplaced and get moved.
    let dest_folders = first_id_by_name(dest.folders.iter().map(|f| (&f.id, &f.name)));
    let dest_collections = first_id_by_name(dest.collections.iter().map(|c| (&c.id, &c.name)));
    let dest_folder_names: HashMap<&str, &str> = dest_folders.iter().map(|(name, id)| (*id, *name)).collect();
    let dest_collection_names: HashMap<&str, &str> =
        dest_collections.iter().map(|(name, id)| (*id, *name)).collect();

    let mut plan = Plan::default();

    let mut entry_by_source_id: HashMap<&str, usize> = HashMap::new();
    for (index, item) in source.items.iter().enumerate() {
        if item.in_trash {
            continue;
        }
        if marker_of(item).is_some() {
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
            Mode::Organization => sorted_names(item.collection_ids.iter().filter_map(|id| source_collections.get(id.as_str()).copied())),
        };
        let marker = Marker {
            source_id: item.id.clone(),
            core_hash: core_hash(item, folder.as_deref(), &collections),
            attachments_hash: Some(attachments_hash(item)),
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
    for (index, item) in dest.items.iter().enumerate() {
        let delete = |reason| Delete { dest: index, reason };
        if item.in_trash {
            plan.deletes.push(delete(DeleteReason::Trashed));
            continue;
        }
        let Some(marker) = marker_of(item) else {
            plan.deletes.push(delete(DeleteReason::Untracked));
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

    for (entry_index, entry) in plan.entries.iter().enumerate() {
        let source_item = &source.items[entry.source];
        let Some((dest_index, marker)) = dest_by_entry.get(&entry_index) else {
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
            Mode::Organization => sorted_names(dest_item.collection_ids.iter().filter_map(|id| dest_collection_names.get(id.as_str()).copied())),
        };
        // The marker alone is not trusted: a destination item edited by hand
        // still carries the old marker.
        let dest_core = core_hash(dest_item, dest_folder, &dest_item_collections);
        let attachments_recorded = match &marker.attachments_hash {
            Some(hash) => *hash == attachments_hash(source_item),
            // A legacy marker says nothing about attachments. That only
            // matters when there are attachments on either side.
            None => source_item.attachments.is_empty() && dest_item.attachments.is_empty(),
        };
        let sync_attachments =
            !attachments_recorded || attachment_names(dest_item) != attachment_names(source_item);
        let write_content = marker != &entry.marker || dest_core != entry.marker.core_hash;
        let set_archived = (dest_item.archived != source_item.archived).then_some(source_item.archived);

        if write_content || sync_attachments || set_archived.is_some() {
            plan.updates.push(Update {
                entry: entry_index,
                dest: *dest_index,
                write_content,
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
                .filter(|f| !wanted.contains(f.name.as_str()) || dest_folders.get(f.name.as_str()) != Some(&f.id.as_str()))
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
                .filter(|c| !wanted.contains_key(c.name.as_str()) || dest_collections.get(c.name.as_str()) != Some(&c.id.as_str()))
                .cloned()
                .collect();
        }
    }
    plan
}

fn names_by_id<'a>(pairs: impl Iterator<Item = (&'a String, &'a String)>) -> HashMap<&'a str, &'a str> {
    pairs.map(|(id, name)| (id.as_str(), name.as_str())).collect()
}

fn first_id_by_name<'a>(pairs: impl Iterator<Item = (&'a String, &'a String)>) -> HashMap<&'a str, &'a str> {
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
    let mut names: Vec<&str> = item.attachments.iter().map(|a| a.file_name.as_str()).collect();
    names.sort_unstable();
    names
}

/// The item's content in a form that is equal for the same item on two servers.
pub fn comparable(item: &Item, folder: Option<&str>, collections: &[String]) -> Value {
    let mut content = item.content.clone();
    if let Some(Node::List(fields)) = content.get_mut("fields") {
        fields.retain(|field| !is_marker_field(field));
    }
    if let Some(Node::Map(login)) = content.get_mut("login") {
        // Servers echo the first URI a second time as `uri` for old clients.
        let has_uris = login.get("uris").and_then(Node::as_list).is_some_and(|uris| !uris.is_empty());
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

fn core_hash(item: &Item, folder: Option<&str>, collections: &[String]) -> String {
    hash(&comparable(item, folder, collections))
}

/// Identifies the source item's attachments. Attachment ids never get reused,
/// so replacing a file under the same name still changes this.
fn attachments_hash(item: &Item) -> String {
    if item.attachments.is_empty() {
        return NO_ATTACHMENTS.to_string();
    }
    let mut identity: Vec<(&str, &str, u64)> = item
        .attachments
        .iter()
        .map(|a| (a.id.as_str(), a.file_name.as_str(), a.size))
        .collect();
    identity.sort_unstable();
    hash(&json!(identity))
}

fn hash(value: &Value) -> String {
    // serde_json keeps insertion order and every map here is built sorted, so
    // equal values serialize identically.
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

/// Limits that stop a misconfigured run before it destroys a vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Safety {
    /// Refuse to delete more than this share of the destination's items.
    pub max_delete_percent: u32,
    /// Below this many deletions the percentage is not checked, so small
    /// vaults can change freely.
    pub delete_floor: usize,
    /// Permit the first run to delete what is already in the destination.
    pub allow_initial_overwrite: bool,
    /// Skip the mass-deletion limit for this run.
    pub allow_mass_delete: bool,
}

impl Default for Safety {
    fn default() -> Self {
        Self {
            max_delete_percent: 50,
            delete_floor: 10,
            allow_initial_overwrite: false,
            allow_mass_delete: false,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SafetyViolation {
    #[error(
        "{0} source item(s) carry a `__source` marker, so the source looks like a mirror destination; \
         check that the source and destination are not swapped"
    )]
    SourceLooksLikeMirror(usize),
    #[error(
        "the destination holds {0} item(s) that this tool did not create and none that it did; \
         a first run would delete them. Pass --allow-initial-overwrite to confirm"
    )]
    InitialOverwrite(usize),
    #[error(
        "the run would delete {deleting} of {total} destination items ({percent}%), above the {limit}% limit; \
         pass --allow-mass-delete if this is expected"
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
    let count = |reason| plan.deletes.iter().filter(|delete| delete.reason == reason).count();
    let untracked = count(DeleteReason::Untracked);
    let stale = count(DeleteReason::Stale);
    if untracked > 0 && plan.tracked_before == 0 && !safety.allow_initial_overwrite {
        return Err(SafetyViolation::InitialOverwrite(untracked));
    }
    let deleting = untracked + stale;
    let total = plan.tracked_before + untracked;
    if !safety.allow_mass_delete && plan.tracked_before > 0 && deleting > safety.delete_floor && total > 0 {
        let percent = u32::try_from(deleting * 100 / total).unwrap_or(u32::MAX);
        if percent > safety.max_delete_percent {
            return Err(SafetyViolation::MassDelete {
                deleting,
                total,
                percent,
                limit: safety.max_delete_percent,
            });
        }
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
                let next = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                diff(l.get(key).unwrap_or(&Value::Null), r.get(key).unwrap_or(&Value::Null), next, out);
            }
        }
        (Value::Array(l), Value::Array(r)) if l.len() == r.len() => {
            for (index, (l, r)) in l.iter().zip(r).enumerate() {
                diff(l, r, format!("{path}[{index}]"), out);
            }
        }
        (l, r) if l == r => {}
        _ => out.push(if path.is_empty() { "<root>".to_string() } else { path }),
    }
}

#[cfg(test)]
mod tests;
