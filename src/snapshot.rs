//! The backup document: a Bitwarden-compatible unencrypted JSON export with
//! one extra block of bookkeeping.
//!
//! Any Bitwarden client can import the document as it is. The extra
//! `vaultMirror` block records what the official format has no place for:
//! which strings were stored unencrypted, and the attachments.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use crate::{
    node::Node,
    plan::is_mirror_marker,
    vault::{Collection, Folder, Scope, Vault, is_content_key},
};

const BLOCK: &str = "vaultMirror";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotAttachment {
    pub item_id: String,
    pub attachment_id: String,
    pub file_name: String,
}

/// Whether an id from a server or a backup is safe as a file name.
pub fn is_path_component(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Builds the backup document for the live items of `vault`. Two documents of
/// an unchanged vault are identical.
///
/// Markers are left out of the items' fields. Left in, they would make the
/// items of an imported export look like copies, which the next run would
/// change or delete.
pub fn build(vault: &Vault, scope: &Scope) -> Result<Value> {
    let organization = scope.organization_id();
    let mut items = Vec::new();
    let mut plain_strings = Map::new();
    let mut attachments = Vec::new();
    let mut live: Vec<_> = vault.items.iter().filter(|item| !item.in_trash).collect();
    live.sort_by(|a, b| a.id.cmp(&b.id));
    for item in live {
        let mut content = item.content.clone();
        if let Some(Node::List(fields)) = content.get_mut("fields") {
            fields.retain(|field| !is_mirror_marker(field));
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
            match organization {
                Some(_) => {
                    let mut ids = item.collection_ids.clone();
                    ids.sort();
                    json!(ids)
                }
                None => Value::Null,
            },
        );
        object.insert("revisionDate".into(), json!(item.revision_date));
        object.insert("creationDate".into(), json!(item.creation_date));
        object.insert("deletedDate".into(), Value::Null);
        object.insert("archivedDate".into(), json!(item.archived_date));
        items.push(Value::Object(object));
        for attachment in &item.attachments {
            attachments.push(SnapshotAttachment {
                item_id: item.id.clone(),
                attachment_id: attachment.id.clone(),
                file_name: attachment.file_name.clone(),
            });
        }
    }
    attachments.sort_by(|a, b| (&a.item_id, &a.attachment_id).cmp(&(&b.item_id, &b.attachment_id)));

    let mut document = Map::new();
    document.insert("encrypted".into(), json!(false));
    match organization {
        Some(_) => {
            let mut collections: Vec<_> = vault.collections.iter().collect();
            collections.sort_by(|a, b| a.id.cmp(&b.id));
            let collections: Vec<Value> = collections.into_iter()
                .map(|c| json!({"id": c.id, "organizationId": organization, "name": c.name, "externalId": c.external_id}))
                .collect();
            document.insert("collections".into(), json!(collections));
        }
        None => {
            let mut folders: Vec<_> = vault.folders.iter().collect();
            folders.sort_by(|a, b| a.id.cmp(&b.id));
            let folders: Vec<Value> = folders
                .into_iter()
                .map(|f| json!({"id": f.id, "name": f.name}))
                .collect();
            document.insert("folders".into(), json!(folders));
        }
    }
    document.insert("items".into(), json!(items));
    document.insert(
        BLOCK.into(),
        json!({
            "scope": if organization.is_some() { "organization" } else { "personal" },
            "plainStrings": plain_strings,
            "attachments": attachments,
        }),
    );
    let document = Value::Object(document);
    // A snapshot that cannot be read back is no backup. Better to fail now,
    // while the vault itself can still be read.
    parse(&document).context("checking the new snapshot")?;
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

    pub fn attachments(&self) -> impl Iterator<Item = &SnapshotAttachment> {
        self.items.iter().flat_map(|item| &item.attachments)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Block {
    scope: ScopeName,
    plain_strings: HashMap<String, Vec<String>>,
    attachments: Vec<SnapshotAttachment>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ScopeName {
    Personal,
    Organization,
}

#[derive(Deserialize)]
struct FolderRecord {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionRecord {
    id: String,
    name: String,
    external_id: Option<String>,
}

/// What an item must have besides its content.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemRecord {
    id: String,
    folder_id: Option<String>,
    collection_ids: Option<Vec<String>>,
    archived_date: Option<String>,
    // Item types a server adds later are numbers too, and their fields are
    // kept as they are.
    #[serde(rename = "type")]
    _kind: u64,
    #[serde(rename = "name")]
    _name: String,
}

/// The list `name` of `document`. A value that is missing or no list is an
/// error, never an empty list.
fn list<T: DeserializeOwned>(document: &Value, name: &str) -> Result<Vec<T>> {
    let list = document
        .get(name)
        .ok_or_else(|| anyhow!("the backup has no {name}"))?;
    // Serde's message would quote the decrypted value it could not read.
    Vec::<T>::deserialize(list).map_err(|_| anyhow!("the backup's {name} are malformed"))
}

/// Reads a backup document. Anything malformed is an error: read as empty, a
/// damaged list would export a vault with parts silently missing.
pub fn parse(document: &Value) -> Result<Restorable> {
    let block: Block = Block::deserialize(
        document
            .get(BLOCK)
            .ok_or_else(|| anyhow!("the backup has no {BLOCK} block"))?,
    )
    .map_err(|_| anyhow!("the backup's {BLOCK} block is malformed"))?;
    let organization = block.scope == ScopeName::Organization;
    let (folders, collections) = match organization {
        false => {
            let folders: Vec<FolderRecord> = list(document, "folders")?;
            let folders = (folders.into_iter())
                .map(|f| Folder {
                    id: f.id,
                    name: f.name,
                    stamp: None,
                })
                .collect();
            (folders, Vec::new())
        }
        true => {
            let collections: Vec<CollectionRecord> = list(document, "collections")?;
            let collections = (collections.into_iter())
                .map(|c| Collection {
                    id: c.id,
                    name: c.name,
                    external_id: c.external_id.filter(|id| !id.is_empty()),
                })
                .collect();
            (Vec::new(), collections)
        }
    };
    let known_folders = unique(folders.iter().map(|f: &Folder| &f.id), "folder")?;
    let known_collections = unique(collections.iter().map(|c: &Collection| &c.id), "collection")?;

    let values: Vec<Value> = list(document, "items")?;
    let mut items = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let read = || -> Result<RestorableItem> {
            let record = ItemRecord::deserialize(value).map_err(|_| anyhow!("it is malformed"))?;
            let paths = (block.plain_strings.get(&record.id))
                .map(Vec::as_slice)
                .unwrap_or_default();
            for path in paths {
                if !value.pointer(path).is_some_and(Value::is_string) {
                    bail!("its list of clear text names a value it does not have");
                }
            }
            let Node::Map(tree) = Node::from_plain(value, paths) else {
                unreachable!("an object reads as a map")
            };
            // In an organization's document the folder is the account's
            // own, which the document doesn't list.
            let folder_id = match record.folder_id {
                Some(id) if !organization => {
                    if !known_folders.contains(&id) {
                        bail!("it is in folder {id}, which the backup does not list");
                    }
                    Some(id)
                }
                _ => None,
            };
            let collection_ids = match organization {
                true => record.collection_ids.unwrap_or_default(),
                false => Vec::new(),
            };
            if let Some(id) = (collection_ids.iter()).find(|id| !known_collections.contains(*id)) {
                bail!("it is in collection {id}, which the backup does not list");
            }
            Ok(RestorableItem {
                id: record.id,
                folder_id,
                collection_ids,
                archived: record.archived_date.is_some_and(|date| !date.is_empty()),
                content: (tree.into_iter())
                    .filter(|(name, node)| is_content_key(name, node))
                    .collect(),
                attachments: Vec::new(),
            })
        };
        items.push(read().with_context(|| format!("item {index} of the backup"))?);
    }
    let ids = unique(items.iter().map(|item| &item.id), "item")?;
    for id in block.plain_strings.keys() {
        if !ids.contains(id) {
            bail!("the backup has a list of clear text of item {id}, which it does not hold");
        }
    }
    let mut attachment_ids = HashSet::new();
    let mut attachments: HashMap<String, Vec<SnapshotAttachment>> = HashMap::new();
    for attachment in block.attachments {
        if !ids.contains(&attachment.item_id) {
            bail!(
                "the backup has an attachment of item {}, which it does not hold",
                attachment.item_id
            );
        }
        if !is_path_component(&attachment.attachment_id)
            || !attachment_ids.insert(attachment.attachment_id.clone())
        {
            bail!(
                "the backup lists attachment {:?} more than once, or under an id that is no \
                 file name",
                attachment.attachment_id
            );
        }
        (attachments.entry(attachment.item_id.clone()).or_default()).push(attachment);
    }
    for item in &mut items {
        item.attachments = attachments.remove(&item.id).unwrap_or_default();
    }
    Ok(Restorable {
        organization,
        folders,
        collections,
        items,
    })
}

fn unique<'a>(ids: impl Iterator<Item = &'a String>, what: &str) -> Result<HashSet<String>> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id.clone()) {
            bail!("the backup lists {what} {id} more than once");
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plan::MARKER_FIELD,
        vault::{Attachment, Item},
    };

    fn marker() -> Node {
        Node::Map(BTreeMap::from([
            ("name".to_string(), Node::secret(MARKER_FIELD)),
            (
                "value".to_string(),
                Node::secret(format!(
                    "{} source-id {} 0 - -",
                    "cd".repeat(32),
                    "ab".repeat(32)
                )),
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
            attachments: vec![Attachment {
                id: "att-1".into(),
                file_name: "a.txt".into(),
                size: 81,
                wrapped_key: None,
            }],
            wrapped_key: None,
        }
    }

    fn vault() -> Vault {
        let mut trashed = item("trashed", "In trash");
        trashed.in_trash = true;
        trashed.attachments[0].id = "att-2".into();
        Vault {
            folders: vec![Folder {
                id: "folder-1".into(),
                name: "Work".into(),
                stamp: None,
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
        }
    }

    #[test]
    fn server_list_order_does_not_change_a_snapshot() {
        let mut vault = vault();
        vault.folders.push(Folder {
            id: "folder-2".into(),
            name: "Other".into(),
            stamp: None,
        });
        vault.collections = vec![
            Collection {
                id: "collection-1".into(),
                name: "Work".into(),
                external_id: None,
            },
            Collection {
                id: "collection-2".into(),
                name: "Other".into(),
                external_id: None,
            },
        ];
        vault.items[0].collection_ids = vec!["collection-2".into(), "collection-1".into()];
        vault.items[0].attachments.push(Attachment {
            id: "att-3".into(),
            file_name: "b.txt".into(),
            size: 5,
            wrapped_key: None,
        });
        let mut second = item("item-2", "Other");
        second.attachments[0].id = "att-4".into();
        vault.items.push(second);
        for scope in [Scope::Personal, Scope::Organization("org-1".into())] {
            let expected = build(&vault, &scope).unwrap();
            let mut reordered = vault.clone();
            reordered.folders.reverse();
            reordered.collections.reverse();
            reordered.items.reverse();
            for item in &mut reordered.items {
                item.attachments.reverse();
                item.collection_ids.reverse();
            }
            let reordered = build(&reordered, &scope).unwrap();
            assert_eq!(reordered, expected);
            let dir = tempfile::tempdir().unwrap();
            let keys = crate::store::Keys::open(dir.path(), "synthetic passphrase").unwrap();
            let store = crate::store::Store::new(dir.path(), "source", &keys);
            let writer = store.begin(time::OffsetDateTime::now_utc()).unwrap();
            for attachment in parse(&expected).unwrap().attachments() {
                writer
                    .write_attachment(&attachment.attachment_id, b"synthetic bytes")
                    .unwrap();
            }
            writer.finish(&expected).unwrap();
            assert!(store.unchanged(&reordered).unwrap());
        }
    }

    #[test]
    fn meaningful_item_list_order_is_preserved_in_snapshots() {
        let mut vault = vault();
        vault.items[0].content.insert(
            "passwordHistory".into(),
            Node::List(vec![
                Node::Map(BTreeMap::from([("password".into(), Node::secret("first"))])),
                Node::Map(BTreeMap::from([(
                    "password".into(),
                    Node::secret("second"),
                )])),
            ]),
        );
        let expected = build(&vault, &Scope::Personal).unwrap();
        for name in ["fields", "passwordHistory"] {
            let mut reordered = vault.clone();
            let Some(Node::List(list)) = reordered.items[0].content.get_mut(name) else {
                unreachable!()
            };
            list.reverse();
            assert_ne!(build(&reordered, &Scope::Personal).unwrap(), expected);
        }
        vault.items[0].revision_date = Some("2026-02-01T00:00:00Z".into());
        assert_ne!(build(&vault, &Scope::Personal).unwrap(), expected);
    }

    #[test]
    fn the_document_is_a_bitwarden_export() {
        let document = build(&vault(), &Scope::Personal).unwrap();
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
        assert_eq!(exported["archivedDate"], "2026-02-02T00:00:00.000000Z");
        assert_eq!(exported["deletedDate"], Value::Null);
        // Markers are left out; the user's own field of the same name stays.
        let fields = exported["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["value"], "1234");
        assert_eq!(fields[1]["value"], "written by hand");
        assert_eq!(
            document[BLOCK]["attachments"],
            json!([{"itemId": "item-1", "attachmentId": "att-1", "fileName": "a.txt"}])
        );
    }

    #[test]
    fn a_document_reads_back_as_the_same_content_without_mirror_markers() {
        let vault = vault();
        let document = build(&vault, &Scope::Personal).unwrap();
        assert_eq!(document, build(&vault, &Scope::Personal).unwrap());
        let restorable = parse(&document).unwrap();

        assert!(!restorable.organization);
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
    fn ordered_markers_are_left_out_of_exports() {
        let mut vault = vault();
        let Some(Node::List(fields)) = vault.items[0].content.get_mut("fields") else {
            panic!()
        };
        let Some(Node::Map(marker)) = fields.last_mut() else {
            panic!()
        };
        let value = format!("{} 42 1", marker["value"].as_str().unwrap());
        marker.insert("value".into(), Node::secret(value));
        let document = build(&vault, &Scope::Personal).unwrap();
        let restored = parse(&document).unwrap();
        let mut expected = vault.items[0].content.clone();
        let Some(Node::List(fields)) = expected.get_mut("fields") else {
            panic!()
        };
        fields.pop();
        assert_eq!(restored.items[0].content, expected);
        assert_eq!(document["items"][0]["fields"].as_array().unwrap().len(), 2);
    }

    /// Clear text after a marker moves up a place when the marker is taken
    /// out, and the list of clear text has to say where it is now.
    #[test]
    fn clear_text_is_listed_where_it_is_once_markers_are_out() {
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
            .insert("fields".into(), Node::List(vec![marker(), dated.clone()]));
        let document = build(&vault, &Scope::Personal).unwrap();
        let paths = &document[BLOCK]["plainStrings"]["item-1"];
        assert!(
            paths
                .as_array()
                .unwrap()
                .contains(&json!("/fields/0/value"))
        );
        let restored = &parse(&document).unwrap().items[0];
        assert_eq!(restored.content["fields"], Node::List(vec![dated]));
    }

    #[test]
    fn containers_with_empty_names_and_empty_lists_are_kept() {
        let mut vault = vault();
        vault.items.clear();
        vault.folders[0].name = String::new();
        let document = build(&vault, &Scope::Personal).unwrap();
        assert_eq!(document["items"], json!([]));
        assert_eq!(parse(&document).unwrap().folders[0].name, "");
    }

    #[test]
    fn content_from_newer_versions_is_kept() {
        let mut document = build(&vault(), &Scope::Personal).unwrap();
        document["items"][0]["futureType"] = json!({"number": "42"});
        let restored = &parse(&document).unwrap().items[0];
        assert_eq!(
            restored.content["futureType"],
            Node::Map(BTreeMap::from([("number".to_string(), Node::secret("42"))]))
        );
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
        let scope = Scope::Organization("org-1".into());
        let document = build(&vault, &scope).unwrap();
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
        // The account's own folder for the item, which an organization's
        // document never lists.
        assert_eq!(document["items"][0]["folderId"], "folder-1");
        assert_eq!(restorable.items[0].folder_id, None);

        vault.items[0].collection_ids.push("gone".into());
        assert!(build(&vault, &scope).is_err(), "it would not read back");
    }

    #[test]
    fn malformed_documents_are_rejected() {
        let base = || build(&vault(), &Scope::Personal).unwrap();
        type Edit = (&'static str, fn(&mut Value));
        let edits: Vec<Edit> = vec![
            ("no block", |d| _ = d.as_object_mut().unwrap().remove(BLOCK)),
            ("items not a list", |d| d["items"] = json!({})),
            ("no items", |d| {
                _ = d.as_object_mut().unwrap().remove("items")
            }),
            ("no folders", |d| {
                _ = d.as_object_mut().unwrap().remove("folders")
            }),
            ("unknown scope", |d| d[BLOCK]["scope"] = json!("team")),
            ("scope without its containers", |d| {
                d[BLOCK]["scope"] = json!("organization")
            }),
            ("attachments not a list", |d| {
                d[BLOCK]["attachments"] = json!({})
            }),
            ("attachment of no item", |d| {
                d[BLOCK]["attachments"][0]["itemId"] = json!("nowhere")
            }),
            ("attachment id as a path", |d| {
                d[BLOCK]["attachments"][0]["attachmentId"] = json!("../key")
            }),
            ("same attachment twice", |d| {
                let record = d[BLOCK]["attachments"][0].clone();
                d[BLOCK]["attachments"].as_array_mut().unwrap().push(record);
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
            ("folder id as a number", |d| {
                d["items"][0]["folderId"] = json!(7)
            }),
            ("folder not listed", |d| {
                d["items"][0]["folderId"] = json!("gone")
            }),
            ("item without a type", |d| {
                _ = d["items"][0].as_object_mut().unwrap().remove("type")
            }),
            ("negative type", |d| d["items"][0]["type"] = json!(-1)),
            ("name as a number", |d| d["items"][0]["name"] = json!(1)),
            ("item without id", |d| {
                _ = d["items"][0].as_object_mut().unwrap().remove("id")
            }),
            ("same item twice", |d| {
                let item = d["items"][0].clone();
                d["items"].as_array_mut().unwrap().push(item);
            }),
            ("folder without a name", |d| {
                d["folders"][0]["name"] = Value::Null
            }),
            ("same folder twice", |d| {
                let folder = d["folders"][0].clone();
                d["folders"].as_array_mut().unwrap().push(folder);
            }),
        ];
        for (what, edit) in edits {
            let mut document = base();
            edit(&mut document);
            assert!(parse(&document).is_err(), "{what}");
        }
    }

    #[test]
    fn errors_about_a_malformed_document_quote_none_of_it() {
        let base = || build(&vault(), &Scope::Personal).unwrap();
        let edits: [fn(&mut Value); 5] = [
            |d| d[BLOCK]["scope"] = json!("hunter2"),
            |d| d[BLOCK]["attachments"] = json!("hunter2"),
            |d| d["folders"] = json!("hunter2"),
            |d| d["items"][0]["type"] = json!("hunter2"),
            |d| d["items"][0]["id"] = json!(["hunter2"]),
        ];
        for edit in edits {
            let mut document = base();
            edit(&mut document);
            let error = format!("{:#}", parse(&document).unwrap_err());
            assert!(!error.contains("hunter2"), "{error}");
        }
    }

    #[test]
    fn path_components_are_plain_ids() {
        let (longest, too_long) = ("a".repeat(128), "a".repeat(129));
        for valid in ["att-1", "20261016T120000Z-1", "A_b", &longest] {
            assert!(is_path_component(valid), "{valid}");
        }
        for invalid in ["", ".", "..", "a/b", "a.age", "a b", &too_long] {
            assert!(!is_path_component(invalid), "{invalid}");
        }
    }
}
