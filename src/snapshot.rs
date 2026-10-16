//! The backup document: a Bitwarden-compatible unencrypted JSON export with
//! one extra block of bookkeeping.
//!
//! Any Bitwarden client can import the document as it is. The extra
//! `vaultMirror` block lets this tool restore it exactly, including
//! attachments, which the official format has no place for.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::{
    node::Node,
    plan::is_mirror_marker,
    vault::{Collection, Folder, Scope, Vault, is_content_key},
};

/// Format 2 gives every snapshot its own attachment files and records their
/// digests. In format 1, snapshots shared one directory of attachment files.
const FORMAT: u64 = 2;
const BLOCK: &str = "vaultMirror";

/// Where the content of a backed-up attachment is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentStorage {
    /// Nowhere: the backup kept only the record.
    Omitted,
    /// In the attachment directory that format 1 snapshots share.
    Legacy,
    /// In the attachment directory of one snapshot, as a file with this digest.
    Snapshot {
        snapshot_id: String,
        ciphertext_sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAttachment {
    pub item_id: String,
    pub attachment_id: String,
    pub file_name: String,
    /// Size of the encrypted file on the server.
    pub size: u64,
    pub storage: AttachmentStorage,
}

/// Whether an id from a server or a backup is safe as a file name.
pub fn is_path_component(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Builds the backup document for the live items of `vault`.
///
/// Markers move from the items' fields into the bookkeeping block. Left in
/// the fields, they would make the items of an imported export look like
/// copies, which the next run would change or delete.
pub fn build(vault: &Vault, scope: &Scope, attachments: &[SnapshotAttachment]) -> Result<Value> {
    let organization = scope.organization_id();
    let mut items = Vec::new();
    let mut plain_strings = Map::new();
    let mut markers = Map::new();
    for item in vault.items.iter().filter(|item| !item.in_trash) {
        let (content, removed) = without_markers(item.content.clone());
        if !removed.is_empty() {
            markers.insert(item.id.clone(), json!(removed));
        }
        let content = Node::Map(content);
        let paths = content.plain_string_paths();
        if !paths.is_empty() {
            plain_strings.insert(item.id.clone(), json!(paths));
        }
        let Value::Object(mut object) = content.to_plain() else {
            unreachable!("a map exports as an object")
        };
        object.insert("id".into(), json!(item.id));
        object.insert("organizationId".into(), json!(organization));
        object.insert("folderId".into(), json!(item.folder_id));
        object.insert(
            "collectionIds".into(),
            if organization.is_some() {
                json!(item.collection_ids)
            } else {
                Value::Null
            },
        );
        object.insert("revisionDate".into(), json!(item.revision_date));
        object.insert("creationDate".into(), json!(item.creation_date));
        object.insert("deletedDate".into(), Value::Null);
        object.insert("archivedDate".into(), json!(item.archived_date));
        items.push(Value::Object(object));
    }

    let mut document = Map::new();
    document.insert("encrypted".into(), json!(false));
    if organization.is_some() {
        let collections: Vec<Value> = vault
            .collections
            .iter()
            .map(|c| json!({"id": c.id, "organizationId": organization, "name": c.name, "externalId": c.external_id}))
            .collect();
        document.insert("collections".into(), json!(collections));
    } else {
        let folders: Vec<Value> = vault
            .folders
            .iter()
            .map(|f| json!({"id": f.id, "name": f.name}))
            .collect();
        document.insert("folders".into(), json!(folders));
    }
    document.insert("items".into(), json!(items));
    let attachments = attachments
        .iter()
        .map(attachment_record)
        .collect::<Result<Vec<_>>>()?;
    document.insert(
        BLOCK.into(),
        json!({
            "format": FORMAT,
            "tool": concat!("vault-mirror ", env!("CARGO_PKG_VERSION")),
            "scope": if organization.is_some() { "organization" } else { "personal" },
            "plainStrings": plain_strings,
            "markers": markers,
            "attachments": attachments,
        }),
    );
    let document = Value::Object(document);
    // A snapshot that restore refuses is no backup. Better to fail now, while
    // the vault itself can still be read.
    parse(&document).context("checking the new snapshot")?;
    Ok(document)
}

fn attachment_record(attachment: &SnapshotAttachment) -> Result<Value> {
    let mut record = json!({
        "itemId": attachment.item_id,
        "attachmentId": attachment.attachment_id,
        "fileName": attachment.file_name,
        "size": attachment.size,
        "stored": attachment.storage != AttachmentStorage::Omitted,
    });
    match &attachment.storage {
        AttachmentStorage::Omitted => {}
        AttachmentStorage::Legacy => bail!(
            "attachment {} is in the directory that older snapshots share; a new snapshot \
             keeps its own files",
            attachment.attachment_id
        ),
        AttachmentStorage::Snapshot {
            snapshot_id,
            ciphertext_sha256,
        } => {
            record["snapshotId"] = json!(snapshot_id);
            record["ciphertextSha256"] = json!(ciphertext_sha256);
        }
    }
    Ok(record)
}

/// `content` without the markers in its fields, and those markers.
fn without_markers(mut content: BTreeMap<String, Node>) -> (BTreeMap<String, Node>, Vec<Value>) {
    let mut markers = Vec::new();
    if let Some(Node::List(fields)) = content.get_mut("fields") {
        fields.retain(|field| {
            let marker = is_mirror_marker(field);
            if marker {
                markers.push(field.to_plain());
            }
            !marker
        });
    }
    (content, markers)
}

/// Stamps a built document with its creation time. Kept apart from [`build`]
/// so that two documents of an unchanged vault are identical before stamping.
pub fn stamp(document: &mut Value, created_at: &str) {
    if let Some(block) = document.get_mut(BLOCK).and_then(Value::as_object_mut) {
        block.insert("createdAt".into(), json!(created_at));
    }
}

/// The document as an offline export: what [`parse`] accepts, with every
/// marker moved from the items' fields into the bookkeeping block, as in
/// [`build`]. Snapshots of format 1 still have markers in their fields.
pub fn importable(document: &Value) -> Result<Value> {
    parse(document)?;
    let mut document = document.clone();
    let plain_strings = document.pointer(&format!("/{BLOCK}/plainStrings")).cloned();
    let mut moved = Vec::new();
    for item in document["items"].as_array_mut().into_iter().flatten() {
        let has_marker = item
            .get("fields")
            .and_then(Value::as_array)
            .is_some_and(|fields| {
                (fields.iter()).any(|field| is_mirror_marker(&Node::from_plain(field, None)))
            });
        if !has_marker {
            continue;
        }
        let id = item.get("id").and_then(Value::as_str).map(str::to_string);
        let paths: Option<Vec<String>> = plain_strings.as_ref().map(|plain| {
            let paths = id.as_ref().and_then(|id| plain.get(id));
            (paths.and_then(Value::as_array).into_iter().flatten())
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
        let Node::Map(tree) = Node::from_plain(item, paths.as_deref()) else {
            unreachable!("parse accepts only objects as items")
        };
        let content = tree
            .into_iter()
            .filter(|(name, node)| is_content_key(name, node))
            .collect();
        let (content, markers) = without_markers(content);
        item["fields"] = content["fields"].to_plain();
        if let Some(id) = id {
            moved.push((id, Node::Map(content).plain_string_paths(), markers));
        }
    }
    // Parsing checked the block's lists, so each has the type matched here.
    if let Some(block) = document.get_mut(BLOCK).and_then(Value::as_object_mut) {
        for (id, paths, markers) in moved {
            if let Some(Value::Object(plain)) = block.get_mut("plainStrings") {
                if paths.is_empty() {
                    plain.remove(&id);
                } else {
                    plain.insert(id.clone(), json!(paths));
                }
            }
            if let Value::Object(evidence) = block.entry("markers").or_insert_with(|| json!({}))
                && let Value::Array(kept) = evidence.entry(id).or_insert_with(|| json!([]))
            {
                kept.extend(markers);
            }
        }
    }
    Ok(document)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestorableItem {
    pub id: String,
    pub folder_id: Option<String>,
    pub collection_ids: Vec<String>,
    pub archived: bool,
    pub content: BTreeMap<String, Node>,
    pub attachments: Vec<SnapshotAttachment>,
}

/// A backup read by [`parse`], the only way to make one, so everything it
/// holds has passed the checks there.
#[derive(Debug, Clone, PartialEq)]
pub struct Restorable {
    organization: bool,
    folders: Vec<Folder>,
    collections: Vec<Collection>,
    items: Vec<RestorableItem>,
}

impl Restorable {
    pub fn organization(&self) -> bool {
        self.organization
    }

    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    pub fn collections(&self) -> &[Collection] {
        &self.collections
    }

    pub fn items(&self) -> &[RestorableItem] {
        &self.items
    }
}

/// The bookkeeping block of a document vault-mirror wrote.
struct Block {
    organization: bool,
    plain_strings: HashMap<String, Vec<String>>,
    markers: HashSet<String>,
    attachments: Vec<SnapshotAttachment>,
}

impl Block {
    fn read(value: &Value) -> Result<Self> {
        let block = value
            .as_object()
            .ok_or_else(|| anyhow!("the backup's {BLOCK} block is not an object"))?;
        let format = match block.get("format").and_then(Value::as_u64) {
            Some(format) if format > FORMAT => {
                bail!("this backup uses format {format}, which needs a newer vault-mirror")
            }
            Some(format @ 1..) => format,
            _ => bail!("the backup's {BLOCK} block names no known format"),
        };
        let organization = match block.get("scope").and_then(Value::as_str) {
            Some("personal") => false,
            Some("organization") => true,
            _ => bail!("the backup's {BLOCK} block names no known scope"),
        };
        let plain_strings = block
            .get("plainStrings")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("the backup has no valid list of clear text"))?
            .iter()
            .map(|(id, paths)| {
                let paths = paths.as_array().and_then(|paths| {
                    (paths.iter())
                        .map(|path| path.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                });
                let paths = paths.ok_or_else(|| {
                    anyhow!("the backup's list of clear text of item {id} is malformed")
                })?;
                Ok((id.clone(), paths))
            })
            .collect::<Result<_>>()?;
        let markers = match block.get("markers") {
            None => HashSet::new(),
            Some(Value::Object(markers)) => {
                for (id, list) in markers {
                    let valid = list
                        .as_array()
                        .is_some_and(|list| list.iter().all(Value::is_object));
                    if !valid {
                        bail!("the backup's markers of item {id} are malformed");
                    }
                }
                markers.keys().cloned().collect()
            }
            Some(_) => bail!("the backup's markers are malformed"),
        };
        let attachments = block
            .get("attachments")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("the backup has no valid list of attachments"))?
            .iter()
            .enumerate()
            .map(|(index, record)| {
                read_attachment(record, format)
                    .with_context(|| format!("attachment record {index} of the backup"))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            organization,
            plain_strings,
            markers,
            attachments,
        })
    }
}

fn read_attachment(record: &Value, format: u64) -> Result<SnapshotAttachment> {
    let text = |name: &str| {
        record
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("it has no valid {name}"))
    };
    let attachment_id = text("attachmentId")?;
    let storage = match (
        record.get("stored").and_then(Value::as_bool),
        record.get("snapshotId"),
        record.get("ciphertextSha256"),
    ) {
        (Some(false), None, None) => AttachmentStorage::Omitted,
        (Some(true), None, None) if format == 1 => AttachmentStorage::Legacy,
        (Some(true), Some(Value::String(snapshot_id)), Some(Value::String(digest)))
            if format >= 2 && is_path_component(snapshot_id) && is_sha256(digest) =>
        {
            AttachmentStorage::Snapshot {
                snapshot_id: snapshot_id.clone(),
                ciphertext_sha256: digest.clone(),
            }
        }
        _ => bail!("it does not say validly where the content is"),
    };
    let names_a_file = storage != AttachmentStorage::Omitted;
    if attachment_id.is_empty() || (names_a_file && !is_path_component(&attachment_id)) {
        bail!("its attachment id is not valid");
    }
    Ok(SnapshotAttachment {
        item_id: text("itemId")?,
        attachment_id,
        file_name: text("fileName")?,
        size: (record.get("size").and_then(Value::as_u64))
            .ok_or_else(|| anyhow!("it has no valid size"))?,
        storage,
    })
}

/// A list the document may leave out. A value that is there but no list is
/// an error, never an empty list.
fn optional_list<'a>(root: &'a Map<String, Value>, name: &str) -> Result<&'a [Value]> {
    match root.get(name) {
        None => Ok(&[]),
        Some(Value::Array(list)) => Ok(list),
        Some(_) => bail!("the backup's {name} are not a list"),
    }
}

/// A string the document may leave out or set to null. Empty means none.
fn optional_text(value: &Value, name: &str) -> Result<Option<String>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone()).filter(|text| !text.is_empty())),
        Some(_) => bail!("its {name} is not text"),
    }
}

fn required_id(value: &Value) -> Result<String> {
    optional_text(value, "id")?.ok_or_else(|| anyhow!("it has no id"))
}

/// Reads a backup document, or a plain unencrypted export written by a
/// Bitwarden client. Anything malformed is an error: read as empty, a damaged
/// list would restore a vault with parts silently missing.
pub fn parse(document: &Value) -> Result<Restorable> {
    let root = document
        .as_object()
        .ok_or_else(|| anyhow!("the backup is not a JSON object"))?;
    match root.get("encrypted") {
        None | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => bail!(
            "this is an encrypted Bitwarden export; only unencrypted JSON exports can be restored"
        ),
        Some(_) => bail!("the backup's encrypted flag is neither true nor false"),
    }
    let block = root.get(BLOCK).map(Block::read).transpose()?;
    let items = root
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("the backup has no list of items"))?;
    let organization = match &block {
        Some(block) => {
            let (expected, other) = match block.organization {
                true => ("collections", "folders"),
                false => ("folders", "collections"),
            };
            if !root.get(expected).is_some_and(Value::is_array) || root.contains_key(other) {
                bail!("the backup's folders and collections do not match its scope");
            }
            block.organization
        }
        None => root.contains_key("collections"),
    };

    let mut folders = Vec::new();
    let mut collections = Vec::new();
    // Servers accept empty folder and collection names, and items refer to
    // them by id, so they are kept.
    let name = |value: &Value| {
        value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    for (index, folder) in optional_list(root, "folders")?.iter().enumerate() {
        let read = || -> Result<Folder> {
            Ok(Folder {
                id: required_id(folder)?,
                name: name(folder).ok_or_else(|| anyhow!("it has no name"))?,
            })
        };
        folders.push(read().with_context(|| format!("folder {index} of the backup"))?);
    }
    for (index, collection) in optional_list(root, "collections")?.iter().enumerate() {
        let read = || -> Result<Collection> {
            Ok(Collection {
                id: required_id(collection)?,
                name: name(collection).ok_or_else(|| anyhow!("it has no name"))?,
                external_id: optional_text(collection, "externalId")?,
            })
        };
        collections.push(read().with_context(|| format!("collection {index} of the backup"))?);
    }
    let folder_ids = unique(folders.iter().map(|f| &f.id), "folder")?;
    let collection_ids = unique(collections.iter().map(|c| &c.id), "collection")?;

    let mut ids = HashSet::new();
    let mut live = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let read = || -> Result<(String, Option<RestorableItem>)> {
            if !item.is_object() {
                bail!("it is not an object");
            }
            let id = match (optional_text(item, "id")?, &block) {
                (Some(id), _) => id,
                // A foreign export without ids still restores.
                (None, None) => format!("item-{index}"),
                (None, Some(_)) => bail!("it has no id"),
            };
            let collections = match item.get("collectionIds") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(list)) => (list.iter())
                    .map(|id| id.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| anyhow!("its collectionIds are not text"))?,
                Some(_) => bail!("its collectionIds are not a list"),
            };
            let folder = optional_text(item, "folderId")?;
            let archived = optional_text(item, "archivedDate")?.is_some();
            if optional_text(item, "deletedDate")?.is_some() {
                return Ok((id, None));
            }
            // Item types a server adds later are numbers too, and their
            // fields are kept as they are.
            let typed = item.get("type").is_some_and(|kind| kind.is_u64());
            if !typed || !item.get("name").is_some_and(Value::is_string) {
                bail!("it has no item type or no name");
            }
            let paths = block.as_ref().map(|block| {
                block
                    .plain_strings
                    .get(&id)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            });
            for path in paths.into_iter().flatten() {
                if !item.pointer(path).is_some_and(Value::is_string) {
                    bail!("its list of clear text names a value it does not have");
                }
            }
            let Node::Map(tree) = Node::from_plain(item, paths) else {
                unreachable!("an object reads as a map")
            };
            let content = tree
                .into_iter()
                .filter(|(name, node)| is_content_key(name, node))
                .collect();
            // A backup of a mirror destination from before format 2 carries
            // markers, which must not end up in a restored vault. A field of
            // the same name that is not a marker is the user's own.
            let (content, _) = without_markers(content);
            // A document vault-mirror wrote lists every folder or collection
            // its items are in. An export from a client may not, and those
            // items restore outside what it left out, as they always have.
            let listed = |id: &String, known: &HashSet<&String>, what: &str| -> Result<bool> {
                let found = known.contains(id);
                if !found && block.is_some() {
                    bail!("it is in {what} {id}, which the backup does not list");
                }
                Ok(found)
            };
            // In an organization's document the folder is the account's own,
            // which the document doesn't list and a restore doesn't set.
            let folder_id = match folder {
                Some(id) if !organization => listed(&id, &folder_ids, "folder")?.then_some(id),
                _ => None,
            };
            let mut kept = Vec::new();
            for id in collections {
                if !organization && block.is_some() {
                    bail!("it is a personal item in collection {id}");
                }
                if organization && listed(&id, &collection_ids, "collection")? {
                    kept.push(id);
                }
            }
            let restorable = RestorableItem {
                id: id.clone(),
                folder_id,
                collection_ids: kept,
                archived,
                content,
                attachments: Vec::new(),
            };
            Ok((id, Some(restorable)))
        };
        let (id, item) = read().with_context(|| format!("item {index} of the backup"))?;
        if !ids.insert(id.clone()) {
            bail!("the backup holds item {id} more than once");
        }
        live.extend(item);
    }

    if let Some(block) = block {
        let known = |id: &String, what: &str| {
            if ids.contains(id) {
                Ok(())
            } else {
                Err(anyhow!(
                    "the backup has {what} of item {id}, which it does not hold"
                ))
            }
        };
        for id in block.plain_strings.keys() {
            known(id, "a list of clear text")?;
        }
        for id in &block.markers {
            known(id, "markers")?;
        }
        let mut attachment_ids = HashSet::new();
        let mut attachments: HashMap<String, Vec<SnapshotAttachment>> = HashMap::new();
        for attachment in block.attachments {
            known(&attachment.item_id, "an attachment")?;
            if !attachment_ids.insert(attachment.attachment_id.clone()) {
                bail!(
                    "the backup lists attachment {} more than once",
                    attachment.attachment_id
                );
            }
            (attachments.entry(attachment.item_id.clone()).or_default()).push(attachment);
        }
        for item in &mut live {
            item.attachments = attachments.remove(&item.id).unwrap_or_default();
        }
    }
    Ok(Restorable {
        organization,
        folders,
        collections,
        items: live,
    })
}

fn unique<'a>(ids: impl Iterator<Item = &'a String>, what: &str) -> Result<HashSet<&'a String>> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            bail!("the backup lists {what} {id} more than once");
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plan::MARKER_FIELD, vault::Item};

    fn marker() -> Node {
        Node::Map(BTreeMap::from([
            ("name".to_string(), Node::secret(MARKER_FIELD)),
            (
                "value".to_string(),
                Node::secret(format!("source-id {} - -", "ab".repeat(32))),
            ),
            ("type".to_string(), Node::Number(1.into())),
        ]))
    }

    fn item(id: &str, name: &str) -> Item {
        let content = BTreeMap::from([
            ("type".to_string(), Node::Number(1.into())),
            ("name".to_string(), Node::secret(name)),
            ("notes".to_string(), Node::Null),
            ("favorite".to_string(), Node::Bool(true)),
            ("reprompt".to_string(), Node::Number(0.into())),
            (
                "login".to_string(),
                Node::Map(BTreeMap::from([
                    ("username".to_string(), Node::secret("user")),
                    ("password".to_string(), Node::secret("2026-01-02T03:04:05Z")),
                    (
                        "passwordRevisionDate".to_string(),
                        Node::Text("2026-01-02T03:04:05.000Z".into()),
                    ),
                ])),
            ),
            (
                "fields".to_string(),
                Node::List(vec![
                    Node::Map(BTreeMap::from([
                        ("name".to_string(), Node::secret("pin")),
                        ("value".to_string(), Node::secret("1234")),
                        ("type".to_string(), Node::Number(1.into())),
                    ])),
                    // The user's own field, which only shares the name.
                    Node::Map(BTreeMap::from([
                        ("name".to_string(), Node::secret(MARKER_FIELD)),
                        ("value".to_string(), Node::secret("written by hand")),
                        ("type".to_string(), Node::Number(1.into())),
                    ])),
                    marker(),
                ]),
            ),
        ]);
        Item {
            id: id.to_string(),
            folder_id: Some("folder-1".into()),
            collection_ids: vec![],
            revision_date: Some("2026-01-01T00:00:00.000000Z".into()),
            creation_date: Some("2025-01-01T00:00:00.000000Z".into()),
            in_trash: false,
            archived: true,
            archived_date: Some("2026-02-02T00:00:00.000000Z".into()),
            content,
            attachments: vec![],
            wrapped_key: None,
        }
    }

    fn vault() -> Vault {
        let mut trashed = item("trashed", "In trash");
        trashed.in_trash = true;
        Vault {
            folders: vec![Folder {
                id: "folder-1".into(),
                name: "Work".into(),
            }],
            items: vec![item("item-1", "GitHub"), trashed],
            ..Vault::default()
        }
    }

    fn attachment() -> SnapshotAttachment {
        SnapshotAttachment {
            item_id: "item-1".into(),
            attachment_id: "att-1".into(),
            file_name: "a.txt".into(),
            size: 81,
            storage: AttachmentStorage::Snapshot {
                snapshot_id: "vault-20261016T120000Z-1".into(),
                ciphertext_sha256: "0f".repeat(32),
            },
        }
    }

    fn fields_of(document: &Value) -> &[Value] {
        document["items"][0]["fields"].as_array().unwrap()
    }

    fn has_marker(fields: &[Value]) -> bool {
        (fields.iter()).any(|field| is_mirror_marker(&Node::from_plain(field, None)))
    }

    #[test]
    fn the_document_is_a_bitwarden_export() {
        let document = build(&vault(), &Scope::Personal, &[attachment()]).unwrap();
        assert_eq!(document["encrypted"], json!(false));
        assert_eq!(
            document["folders"],
            json!([{"id": "folder-1", "name": "Work"}])
        );
        assert!(document.get("collections").is_none());
        let items = document["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            1,
            "trash is left out, as in an official export"
        );
        let exported = &items[0];
        assert_eq!(exported["id"], "item-1");
        assert_eq!(exported["type"], 1);
        assert_eq!(exported["name"], "GitHub");
        assert_eq!(exported["folderId"], "folder-1");
        assert_eq!(exported["organizationId"], Value::Null);
        assert_eq!(exported["collectionIds"], Value::Null);
        assert_eq!(exported["login"]["username"], "user");
        assert_eq!(exported["fields"][0]["value"], "1234");
        assert_eq!(exported["archivedDate"], "2026-02-02T00:00:00.000000Z");
        assert_eq!(exported["deletedDate"], Value::Null);
        assert_eq!(
            document[BLOCK]["attachments"][0],
            json!({
                "itemId": "item-1",
                "attachmentId": "att-1",
                "fileName": "a.txt",
                "size": 81,
                "stored": true,
                "snapshotId": "vault-20261016T120000Z-1",
                "ciphertextSha256": "0f".repeat(32),
            })
        );
    }

    #[test]
    fn a_document_restores_to_the_same_content_without_mirror_markers() {
        let vault = vault();
        let mut document = build(&vault, &Scope::Personal, &[attachment()]).unwrap();
        stamp(&mut document, "2026-10-14T00:00:00Z");
        let restorable = parse(&document).unwrap();

        assert!(!restorable.organization);
        assert_eq!(document[BLOCK]["createdAt"], "2026-10-14T00:00:00Z");
        assert_eq!(restorable.folders, vault.folders);
        assert_eq!(restorable.items.len(), 1);
        let restored = &restorable.items[0];
        assert!(restored.archived);
        assert_eq!(restored.folder_id.as_deref(), Some("folder-1"));
        assert_eq!(restored.attachments, [attachment()]);
        let mut expected = vault.items[0].content.clone();
        let Some(Node::List(fields)) = expected.get_mut("fields") else {
            panic!()
        };
        fields.pop();
        assert_eq!(restored.content, expected);
    }

    #[test]
    fn snapshots_keep_markers_out_of_item_fields() {
        let document = build(&vault(), &Scope::Personal, &[]).unwrap();
        let fields = fields_of(&document);
        assert_eq!(fields.len(), 2, "the pin and the user's own field");
        assert_eq!(fields[1]["value"], "written by hand");
        assert!(!has_marker(fields));
        let evidence = &document[BLOCK]["markers"]["item-1"];
        assert_eq!(evidence.as_array().map(Vec::len), Some(1));
        assert_eq!(evidence[0]["name"], MARKER_FIELD);
    }

    /// Clear text after a marker moves up a place when the marker is taken
    /// out, and the list of clear text has to say where it is now.
    fn vault_with_clear_text_after_a_marker() -> Vault {
        let mut vault = vault();
        let dated = Node::Map(BTreeMap::from([
            ("name".to_string(), Node::secret("since")),
            (
                "value".to_string(),
                Node::Text("2026-03-04T05:06:07.000Z".into()),
            ),
            ("type".to_string(), Node::Number(0.into())),
        ]));
        vault.items[0]
            .content
            .insert("fields".into(), Node::List(vec![marker(), dated]));
        vault
    }

    #[test]
    fn clear_text_is_listed_where_it_is_once_markers_are_out() {
        let vault = vault_with_clear_text_after_a_marker();
        let document = build(&vault, &Scope::Personal, &[]).unwrap();
        let paths = &document[BLOCK]["plainStrings"]["item-1"];
        assert!(
            paths
                .as_array()
                .unwrap()
                .contains(&json!("/fields/0/value"))
        );
        let restored = &parse(&document).unwrap().items[0];
        assert_eq!(
            restored.content["fields"],
            Node::List(vec![Node::Map(BTreeMap::from([
                ("name".to_string(), Node::secret("since")),
                (
                    "value".to_string(),
                    Node::Text("2026-03-04T05:06:07.000Z".into())
                ),
                ("type".to_string(), Node::Number(0.into())),
            ]))])
        );
    }

    /// A document as vault-mirror wrote it before format 2: markers in the
    /// fields, clear text listed with the markers in place, and attachments
    /// in the shared directory.
    fn format_1(vault: &Vault) -> Value {
        let mut document = build(vault, &Scope::Personal, &[]).unwrap();
        let item = &vault.items[0];
        let content = Node::Map(item.content.clone());
        document["items"][0]["fields"] = content.to_plain()["fields"].clone();
        let block = document[BLOCK].as_object_mut().unwrap();
        block.remove("markers");
        block.insert("format".into(), json!(1));
        let mut plain = Map::new();
        plain.insert(item.id.clone(), json!(content.plain_string_paths()));
        block.insert("plainStrings".into(), Value::Object(plain));
        block.insert(
            "attachments".into(),
            json!([
                {"itemId": "item-1", "attachmentId": "att-1", "fileName": "a.txt", "size": 81, "stored": true},
                {"itemId": "item-1", "attachmentId": "att-2", "fileName": "b.txt", "size": 9, "stored": false},
            ]),
        );
        document
    }

    #[test]
    fn a_format_1_document_restores_without_markers_from_the_shared_directory() {
        let vault = vault_with_clear_text_after_a_marker();
        let document = format_1(&vault);
        assert!(has_marker(fields_of(&document)));
        let restored = &parse(&document).unwrap().items[0];
        let storage: Vec<_> = (restored.attachments.iter())
            .map(|a| a.storage.clone())
            .collect();
        assert_eq!(
            storage,
            [AttachmentStorage::Legacy, AttachmentStorage::Omitted]
        );
        assert_eq!(
            restored.content,
            parse(&build(&vault, &Scope::Personal, &[]).unwrap())
                .unwrap()
                .items[0]
                .content
        );
    }

    #[test]
    fn an_export_has_no_markers_in_its_fields() {
        for document in [
            format_1(&vault_with_clear_text_after_a_marker()),
            format_1(&vault()),
            build(&vault(), &Scope::Personal, &[]).unwrap(),
        ] {
            let export = importable(&document).unwrap();
            assert!(!has_marker(fields_of(&export)));
            assert_eq!(
                export[BLOCK]["markers"]["item-1"].as_array().map(Vec::len),
                Some(1)
            );
            assert_eq!(parse(&export).unwrap(), parse(&document).unwrap());
        }
        // The user's own field of the same name stays.
        let export = importable(&format_1(&vault())).unwrap();
        assert_eq!(fields_of(&export)[1]["value"], "written by hand");

        let mut foreign = format_1(&vault());
        foreign.as_object_mut().unwrap().remove(BLOCK);
        let export = importable(&foreign).unwrap();
        assert!(!has_marker(fields_of(&export)));
        assert!(export.get(BLOCK).is_none());
    }

    #[test]
    fn containers_with_empty_names_and_empty_lists_are_kept() {
        let document = json!({
            "encrypted": false,
            "collections": [{ "id": "c1", "organizationId": "o", "name": "" }],
            "items": [],
        });
        let restorable = parse(&document).unwrap();
        assert_eq!(restorable.collections.len(), 1);
        assert_eq!(restorable.collections[0].name, "");

        let mut vault = vault();
        vault.items.clear();
        vault.folders[0].name = String::new();
        let document = build(&vault, &Scope::Personal, &[]).unwrap();
        assert_eq!(document["items"], json!([]));
        assert_eq!(parse(&document).unwrap().folders[0].name, "");
    }

    #[test]
    fn content_and_bookkeeping_from_newer_versions_are_kept() {
        let mut document = build(&vault(), &Scope::Personal, &[attachment()]).unwrap();
        document["items"][0]["futureType"] = json!({"number": "42"});
        document[BLOCK]["origin"] = json!({"apiUrl": "https://example.com/api"});
        let restored = &parse(&document).unwrap().items[0];
        assert_eq!(
            restored.content["futureType"],
            Node::Map(BTreeMap::from([("number".to_string(), Node::secret("42"))]))
        );
    }

    #[test]
    fn unstamped_documents_of_an_unchanged_vault_are_identical() {
        let first = build(&vault(), &Scope::Personal, &[attachment()]).unwrap();
        let second = build(&vault(), &Scope::Personal, &[attachment()]).unwrap();
        assert_eq!(first.to_string(), second.to_string());
        assert!(first[BLOCK].get("createdAt").is_none());
    }

    #[test]
    fn organization_documents_carry_collections() {
        let mut vault = vault();
        vault.folders.clear();
        vault.collections = vec![Collection {
            id: "col-1".into(),
            name: "Team".into(),
            external_id: Some("ext".into()),
        }];
        vault.items[0].collection_ids = vec!["col-1".into()];
        let document = build(&vault, &Scope::Organization("org-1".into()), &[]).unwrap();
        assert_eq!(
            document["collections"],
            json!([{"id": "col-1", "organizationId": "org-1", "name": "Team", "externalId": "ext"}])
        );
        assert_eq!(document["items"][0]["organizationId"], "org-1");
        assert_eq!(document["items"][0]["collectionIds"], json!(["col-1"]));
        let restorable = parse(&document).unwrap();
        assert!(restorable.organization);
        assert_eq!(restorable.collections, vault.collections);
        assert_eq!(restorable.items[0].collection_ids, ["col-1"]);
        assert_eq!(restorable.items[0].folder_id, None);
    }

    #[test]
    fn only_a_client_export_may_name_containers_it_does_not_list() {
        let mut personal = format_1(&vault());
        personal["items"][0]["folderId"] = json!("gone");
        let mut vault = vault();
        vault.folders.clear();
        vault.collections = vec![Collection {
            id: "col-1".into(),
            name: "Team".into(),
            external_id: None,
        }];
        vault.items[0].collection_ids = vec!["col-1".into(), "gone".into()];
        let scope = Scope::Organization("org".into());
        assert!(build(&vault, &scope, &[]).is_err(), "it would not restore");
        vault.items[0].collection_ids.pop();
        let mut organization = build(&vault, &scope, &[]).unwrap();
        // The account's own folder for the item, which an organization's
        // document never lists.
        assert_eq!(organization["items"][0]["folderId"], "folder-1");
        assert!(parse(&organization).is_ok());
        organization["items"][0]["collectionIds"] = json!(["col-1", "gone"]);

        for (native, collections) in [(personal, vec![]), (organization, vec!["col-1"])] {
            assert!(parse(&native).is_err(), "{native}");
            let mut foreign = native;
            foreign.as_object_mut().unwrap().remove(BLOCK);
            let restored = &parse(&foreign).unwrap().items[0];
            assert_eq!(restored.folder_id, None);
            assert_eq!(restored.collection_ids, collections);
        }
    }

    #[test]
    fn an_export_from_a_bitwarden_client_is_restorable() {
        let export = json!({
            "encrypted": false,
            "folders": [{"id": "f", "name": "Folder"}],
            "items": [
                {
                    "id": "i", "organizationId": null, "folderId": "f", "type": 1, "reprompt": 0,
                    "name": "Site", "notes": "n", "favorite": false, "collectionIds": null,
                    "revisionDate": "2026-01-01T00:00:00.000Z", "creationDate": "2026-01-01T00:00:00.000Z", "deletedDate": null,
                    "login": {"username": "u", "password": "p", "totp": null, "uris": [{"match": null, "uri": "https://x"}]},
                    "passwordHistory": [{"lastUsedDate": "2025-01-01T00:00:00.000Z", "password": "old"}],
                },
                {"id": "gone", "type": 2, "name": "Deleted", "deletedDate": "2026-01-01T00:00:00.000Z", "secureNote": {"type": 0}},
            ],
        });
        let restorable = parse(&export).unwrap();
        assert_eq!(restorable.items.len(), 1);
        assert_eq!(restorable.items[0].folder_id.as_deref(), Some("f"));
        let content = &restorable.items[0].content;
        assert_eq!(content["name"], Node::secret("Site"));
        assert_eq!(
            content["login"].as_map().unwrap()["password"],
            Node::secret("p")
        );
        let history = content["passwordHistory"].as_list().unwrap()[0]
            .as_map()
            .unwrap();
        assert_eq!(
            history["lastUsedDate"],
            Node::Text("2025-01-01T00:00:00.000Z".into())
        );
        assert!(!content.contains_key("revisionDate") && !content.contains_key("collectionIds"));
    }

    #[test]
    fn unusable_documents_are_rejected_with_a_reason() {
        assert!(
            parse(&json!({"encrypted": true, "data": "x"}))
                .unwrap_err()
                .to_string()
                .contains("encrypted")
        );
        assert!(
            parse(&json!({"vaultMirror": {"format": 99}, "items": []}))
                .unwrap_err()
                .to_string()
                .contains("newer")
        );
        assert!(parse(&json!([])).is_err());
    }

    #[test]
    fn documents_without_a_usable_item_list_are_rejected() {
        for document in [
            json!({}),
            json!({"encrypted": false}),
            json!({"encrypted": "no", "items": []}),
            json!({"items": {}}),
            json!({"items": null}),
            json!({"items": ["x"]}),
            json!({"items": [], "folders": {}}),
            json!({"items": [], "collections": "x"}),
            json!({"items": [], "folders": [{"name": "no id"}]}),
            json!({"items": [{"id": "only an id"}]}),
            json!({"items": [{"type": 1}]}),
            json!({"items": [{"type": "1", "name": "n"}]}),
            json!({"items": [], "folders": [{"id": "f", "name": "a"}, {"id": "f", "name": "b"}]}),
        ] {
            assert!(parse(&document).is_err(), "{document}");
        }
    }

    #[test]
    fn malformed_bookkeeping_is_rejected() {
        let base = || build(&vault(), &Scope::Personal, &[attachment()]).unwrap();
        type Edit = (&'static str, fn(&mut Value));
        let edits: Vec<Edit> = vec![
            ("no block format", |d| d[BLOCK] = json!({})),
            ("attachments not a list", |d| {
                d[BLOCK]["attachments"] = json!({})
            }),
            ("attachment of no item", |d| {
                d[BLOCK]["attachments"][0]["itemId"] = json!("nowhere")
            }),
            ("attachment id as a path", |d| {
                d[BLOCK]["attachments"][0]["attachmentId"] = json!("../key")
            }),
            ("size as text", |d| {
                d[BLOCK]["attachments"][0]["size"] = json!("81")
            }),
            ("stored as text", |d| {
                d[BLOCK]["attachments"][0]["stored"] = json!("yes")
            }),
            ("plain strings as a list", |d| {
                d[BLOCK]["plainStrings"] = json!([])
            }),
            ("plain strings of no item", |d| {
                d[BLOCK]["plainStrings"]["nowhere"] = json!([])
            }),
            ("plain string nowhere", |d| {
                d[BLOCK]["plainStrings"]["item-1"] = json!(["/missing"])
            }),
            ("plain string at an object", |d| {
                d[BLOCK]["plainStrings"]["item-1"] = json!(["/login"])
            }),
            ("markers as a list", |d| d[BLOCK]["markers"] = json!([])),
            ("markers of no item", |d| {
                d[BLOCK]["markers"]["nowhere"] = json!([])
            }),
            ("format 0", |d| d[BLOCK]["format"] = json!(0)),
            ("format as text", |d| d[BLOCK]["format"] = json!("1")),
            ("unknown scope", |d| d[BLOCK]["scope"] = json!("team")),
            ("scope without its containers", |d| {
                d[BLOCK]["scope"] = json!("organization")
            }),
            ("collection id as a number", |d| {
                d["items"][0]["collectionIds"] = json!([1])
            }),
            ("folder id as a number", |d| {
                d["items"][0]["folderId"] = json!(7)
            }),
            ("archive date as a number", |d| {
                d["items"][0]["archivedDate"] = json!(7)
            }),
            ("item without a type", |d| {
                d["items"][0].as_object_mut().unwrap().remove("type");
            }),
            ("negative type", |d| d["items"][0]["type"] = json!(-1)),
            ("name as a number", |d| d["items"][0]["name"] = json!(1)),
            ("personal item in a collection", |d| {
                d["items"][0]["collectionIds"] = json!(["c"])
            }),
            ("item without id", |d| {
                d["items"][0].as_object_mut().unwrap().remove("id");
            }),
            ("same item twice", |d| {
                let item = d["items"][0].clone();
                d["items"].as_array_mut().unwrap().push(item);
            }),
            ("folder without a name", |d| {
                d["folders"][0]["name"] = Value::Null
            }),
            ("same attachment twice", |d| {
                let record = d[BLOCK]["attachments"][0].clone();
                d[BLOCK]["attachments"].as_array_mut().unwrap().push(record);
            }),
            ("stored without a snapshot", |d| {
                d[BLOCK]["attachments"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("snapshotId");
            }),
            ("left out but naming a file", |d| {
                d[BLOCK]["attachments"][0]["stored"] = json!(false)
            }),
            ("snapshot id as a path", |d| {
                d[BLOCK]["attachments"][0]["snapshotId"] = json!("..")
            }),
            ("digest in capitals", |d| {
                d[BLOCK]["attachments"][0]["ciphertextSha256"] = json!("0F".repeat(32))
            }),
            ("digest too short", |d| {
                d[BLOCK]["attachments"][0]["ciphertextSha256"] = json!("0f")
            }),
            ("format 1 naming a snapshot", |d| {
                d[BLOCK]["format"] = json!(1)
            }),
        ];
        let mut valid = base();
        valid[BLOCK]["attachments"][0]["fileName"] = json!("");
        assert!(parse(&valid).is_ok(), "an empty file name is valid");
        for (what, edit) in edits {
            let mut document = base();
            edit(&mut document);
            assert!(parse(&document).is_err(), "{what}");
        }
    }

    #[test]
    fn a_new_snapshot_cannot_refer_to_the_shared_directory() {
        let mut legacy = attachment();
        legacy.storage = AttachmentStorage::Legacy;
        assert!(build(&vault(), &Scope::Personal, &[legacy]).is_err());
        let mut unsafe_id = attachment();
        unsafe_id.attachment_id = "../../key".into();
        assert!(build(&vault(), &Scope::Personal, &[unsafe_id]).is_err());
        let mut omitted = attachment();
        omitted.storage = AttachmentStorage::Omitted;
        let document = build(&vault(), &Scope::Personal, &[omitted.clone()]).unwrap();
        assert_eq!(parse(&document).unwrap().items[0].attachments, [omitted]);
    }

    #[test]
    fn path_components_are_plain_ids() {
        let (longest, too_long) = ("a".repeat(128), "a".repeat(129));
        for valid in ["att-1", "vault-20261016T120000Z-1", "A_b", &longest] {
            assert!(is_path_component(valid), "{valid}");
        }
        for invalid in ["", ".", "..", "a/b", "a.age", "a b", &too_long] {
            assert!(!is_path_component(invalid), "{invalid}");
        }
    }
}
