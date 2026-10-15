//! The backup document: a Bitwarden-compatible unencrypted JSON export with
//! one extra block of bookkeeping.
//!
//! Any Bitwarden client can import the document as it is. The extra
//! `vaultMirror` block lets this tool restore it exactly, including
//! attachments, which the official format has no place for.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::{
    node::Node,
    plan::is_mirror_marker,
    vault::{Collection, Folder, Scope, Vault, is_content_key},
};

const FORMAT: u64 = 1;
const BLOCK: &str = "vaultMirror";

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotAttachment {
    pub item_id: String,
    pub attachment_id: String,
    pub file_name: String,
    /// Size of the encrypted file on the server.
    pub size: u64,
    /// Whether the file's content is in the backup, or only this record.
    pub stored: bool,
}

/// Builds the backup document for the live items of `vault`.
pub fn build(vault: &Vault, scope: &Scope, attachments: &[SnapshotAttachment]) -> Value {
    let organization = scope.organization_id();
    let mut items = Vec::new();
    let mut plain_strings = Map::new();
    for item in vault.items.iter().filter(|item| !item.in_trash) {
        let content = Node::Map(item.content.clone());
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
    let attachments: Vec<Value> = attachments
        .iter()
        .map(|a| {
            json!({
                "itemId": a.item_id,
                "attachmentId": a.attachment_id,
                "fileName": a.file_name,
                "size": a.size,
                "stored": a.stored,
            })
        })
        .collect();
    document.insert(
        BLOCK.into(),
        json!({
            "format": FORMAT,
            "tool": concat!("vault-mirror ", env!("CARGO_PKG_VERSION")),
            "scope": if organization.is_some() { "organization" } else { "personal" },
            "plainStrings": plain_strings,
            "attachments": attachments,
        }),
    );
    Value::Object(document)
}

/// Stamps a built document with its creation time. Kept apart from [`build`]
/// so that two documents of an unchanged vault are identical before stamping.
pub fn stamp(document: &mut Value, created_at: &str) {
    if let Some(block) = document.get_mut(BLOCK).and_then(Value::as_object_mut) {
        block.insert("createdAt".into(), json!(created_at));
    }
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Restorable {
    pub organization: bool,
    pub folders: Vec<Folder>,
    pub collections: Vec<Collection>,
    pub items: Vec<RestorableItem>,
}

/// Reads a backup document, or a plain unencrypted export written by a
/// Bitwarden client.
pub fn parse(document: &Value) -> Result<Restorable> {
    let root = document
        .as_object()
        .ok_or_else(|| anyhow!("the backup is not a JSON object"))?;
    if root.get("encrypted").and_then(Value::as_bool) == Some(true) {
        bail!(
            "this is an encrypted Bitwarden export; only unencrypted JSON exports can be restored"
        );
    }
    let block = root.get(BLOCK);
    if let Some(format) = block.and_then(|b| b.get("format")).and_then(Value::as_u64)
        && format > FORMAT
    {
        bail!("this backup uses format {format}, which needs a newer vault-mirror");
    }
    let list = |name: &str| {
        root.get(name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let text = |value: &Value, name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let mut restorable = Restorable {
        organization: root.contains_key("collections")
            || block.and_then(|b| b.get("scope")).and_then(Value::as_str) == Some("organization"),
        ..Restorable::default()
    };
    // Servers accept empty folder and collection names, and items refer to
    // them by id, so they are kept.
    let name = |value: &Value| {
        value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    for folder in list("folders") {
        if let (Some(id), Some(name)) = (text(&folder, "id"), name(&folder)) {
            restorable.folders.push(Folder { id, name });
        }
    }
    for collection in list("collections") {
        if let (Some(id), Some(name)) = (text(&collection, "id"), name(&collection)) {
            restorable.collections.push(Collection {
                id,
                name,
                external_id: text(&collection, "externalId"),
            });
        }
    }

    let mut attachments: BTreeMap<String, Vec<SnapshotAttachment>> = BTreeMap::new();
    for attachment in block
        .and_then(|b| b.get("attachments"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |name: &str| {
            text(attachment, name).ok_or_else(|| anyhow!("a backed-up attachment has no {name}"))
        };
        let entry = SnapshotAttachment {
            item_id: field("itemId")?,
            attachment_id: field("attachmentId")?,
            file_name: field("fileName")?,
            size: attachment.get("size").and_then(Value::as_u64).unwrap_or(0),
            stored: attachment
                .get("stored")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        attachments
            .entry(entry.item_id.clone())
            .or_default()
            .push(entry);
    }
    let plain_strings = block
        .and_then(|b| b.get("plainStrings"))
        .and_then(Value::as_object);

    for (index, item) in list("items").iter().enumerate() {
        let id = text(item, "id").unwrap_or_else(|| format!("item-{index}"));
        if text(item, "deletedDate").is_some() {
            continue;
        }
        // Without the bookkeeping block this is a foreign export, and which
        // strings are clear text has to be inferred.
        let paths: Option<Vec<String>> = block.map(|_| {
            plain_strings
                .and_then(|map| map.get(&id))
                .and_then(Value::as_array)
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        });
        let Node::Map(tree) = Node::from_plain(item, paths.as_deref()) else {
            bail!("backup item {id} is not an object");
        };
        let mut content: BTreeMap<String, Node> = tree
            .into_iter()
            .filter(|(name, node)| is_content_key(name, node))
            .collect();
        if let Some(Node::List(fields)) = content.get_mut("fields") {
            // A backup of a mirror destination carries mirror markers, which
            // must not end up in a restored vault. A field of the same name
            // that is not a marker is the user's own.
            fields.retain(|field| !is_mirror_marker(field));
        }
        restorable.items.push(RestorableItem {
            folder_id: text(item, "folderId"),
            collection_ids: item
                .get("collectionIds")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            archived: text(item, "archivedDate").is_some(),
            content,
            attachments: attachments.remove(&id).unwrap_or_default(),
            id,
        });
    }
    Ok(restorable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plan::MARKER_FIELD, vault::Item};

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
                    Node::Map(BTreeMap::from([
                        ("name".to_string(), Node::secret(MARKER_FIELD)),
                        (
                            "value".to_string(),
                            Node::secret(format!("source-id {} -", "ab".repeat(32))),
                        ),
                        ("type".to_string(), Node::Number(1.into())),
                    ])),
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
            stored: true,
        }
    }

    #[test]
    fn the_document_is_a_bitwarden_export() {
        let document = build(&vault(), &Scope::Personal, &[attachment()]);
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
    }

    #[test]
    fn a_document_restores_to_the_same_content_without_mirror_markers() {
        let vault = vault();
        let mut document = build(&vault, &Scope::Personal, &[attachment()]);
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
    fn containers_with_empty_names_are_kept() {
        let document = json!({
            "encrypted": false,
            "collections": [{ "id": "c1", "organizationId": "o", "name": "" }],
            "items": [],
        });
        let restorable = parse(&document).unwrap();
        assert_eq!(restorable.collections.len(), 1);
        assert_eq!(restorable.collections[0].name, "");
    }

    #[test]
    fn unstamped_documents_of_an_unchanged_vault_are_identical() {
        let first = build(&vault(), &Scope::Personal, &[attachment()]);
        let second = build(&vault(), &Scope::Personal, &[attachment()]);
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
        let document = build(&vault, &Scope::Organization("org-1".into()), &[]);
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
}
