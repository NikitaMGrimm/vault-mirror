//! A decrypted view of one account's vault, and the operations that change it.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::{
    api::{self, ApiCredentials, ServerUrls, Session},
    crypto::{KdfParams, MasterSecret, SymKey},
    node::Node,
};

/// Item fields that identify or describe an item on one particular server.
/// They are tracked as typed fields on [`Item`], never as content.
const META_KEYS: &[&str] = &[
    "id",
    "object",
    "organizationId",
    "folderId",
    "collectionIds",
    "revisionDate",
    "creationDate",
    "deletedDate",
    "archivedDate",
    "key",
    "attachments",
    "attachments2",
    // A legacy duplicate of the type-specific block that Vaultwarden still sends.
    "data",
    "edit",
    "viewPassword",
    "permissions",
    "organizationUseTotp",
    "encryptedFor",
    "lastKnownRevisionDate",
];

/// Scalar content every item type has. Type-specific data lives in objects
/// such as `login` or `card`, which are all treated as content.
const SCALAR_CONTENT_KEYS: &[&str] = &["type", "name", "notes", "favorite", "reprompt"];
const LIST_CONTENT_KEYS: &[&str] = &["fields", "passwordHistory"];

/// The largest batch the Bitwarden server accepts for bulk item operations.
const BULK_LIMIT: usize = 500;

/// Which part of an account a run works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The individual vault: every item that belongs to no organization.
    Personal,
    /// One organization's vault, as far as this account can see it.
    Organization(String),
}

impl Scope {
    pub fn organization_id(&self) -> Option<&str> {
        match self {
            Self::Personal => None,
            Self::Organization(id) => Some(id),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Folder {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Collection {
    pub id: String,
    pub name: String,
    pub external_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub file_name: String,
    pub size: u64,
    /// The attachment's own key, wrapped with the item's key. Attachments
    /// from very old clients have none and use the item's key directly.
    pub(crate) wrapped_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub folder_id: Option<String>,
    pub collection_ids: Vec<String>,
    pub revision_date: Option<String>,
    pub creation_date: Option<String>,
    pub in_trash: bool,
    pub archived: bool,
    /// When the item was archived, as the server reports it.
    pub archived_date: Option<String>,
    pub content: BTreeMap<String, Node>,
    pub attachments: Vec<Attachment>,
    /// The item's own key as stored on the server, when it has one.
    pub(crate) wrapped_key: Option<String>,
}

impl Item {
    pub fn name(&self) -> &str {
        self.content
            .get("name")
            .and_then(Node::as_str)
            .unwrap_or("")
    }

    pub fn fields(&self) -> &[Node] {
        self.content
            .get("fields")
            .and_then(Node::as_list)
            .unwrap_or(&[])
    }
}

/// What to write when creating or replacing an item.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub content: BTreeMap<String, Node>,
    pub folder_id: Option<String>,
    pub collection_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Vault {
    pub folders: Vec<Folder>,
    pub collections: Vec<Collection>,
    pub items: Vec<Item>,
}

struct Keys {
    user: SymKey,
    organizations: HashMap<String, SymKey>,
}

/// One unlocked account on one server.
pub struct Account {
    session: Session,
    keys: Keys,
    pub user_id: String,
    pub email: String,
}

impl Account {
    pub async fn open(
        http: reqwest::Client,
        urls: ServerUrls,
        credentials: ApiCredentials,
        master_password: &str,
    ) -> Result<Self> {
        let session = Session::login(http.clone(), urls.clone(), credentials).await?;
        let profile = session.get("/accounts/profile").await?;
        let text = |name: &str| {
            profile
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("the account profile has no {name}"))
        };
        let user_id = text("id")?;
        let email = text("email")?;

        let kdf = match session.login_kdf {
            Some(kdf) => Some(kdf),
            None => api::prelogin(&http, &urls, &email).await?,
        };
        let (kind, iterations, memory, parallelism) =
            kdf.ok_or_else(|| anyhow!("the server did not report key derivation settings"))?;
        let kdf = KdfParams::from_server(kind, iterations, memory, parallelism)?;

        // Key derivation is deliberately expensive; keep it off the async workers.
        let password = master_password.to_string();
        let salt = email.clone();
        let master =
            tokio::task::spawn_blocking(move || MasterSecret::derive(&password, &salt, &kdf))
                .await
                .context("key derivation task failed")??;
        let user = master.unlock_user_key(&text("key")?)?;

        let mut organizations = HashMap::new();
        let memberships = profile.get("organizations").and_then(Value::as_array);
        if memberships.is_some_and(|list| !list.is_empty()) {
            let private = user.unwrap_private_key(&text("privateKey")?)?;
            for membership in memberships.into_iter().flatten() {
                let (Some(id), Some(key)) = (
                    membership.get("id").and_then(Value::as_str),
                    membership.get("key").and_then(Value::as_str),
                ) else {
                    continue;
                };
                organizations.insert(id.to_string(), private.unwrap_shared_key(key)?);
            }
        }

        Ok(Self {
            session,
            keys: Keys {
                user,
                organizations,
            },
            user_id,
            email,
        })
    }

    pub fn api_url(&self) -> &str {
        self.session.api_url()
    }

    fn scope_key(&self, scope: &Scope) -> Result<&SymKey> {
        match scope {
            Scope::Personal => Ok(&self.keys.user),
            Scope::Organization(id) => self.keys.organizations.get(id).ok_or_else(|| {
                anyhow!("this account is not a confirmed member of organization {id}")
            }),
        }
    }

    /// The key an existing item's content is encrypted with.
    fn item_key(&self, scope: &Scope, item: &Item) -> Result<ItemKey<'_>> {
        let scope_key = self.scope_key(scope)?;
        Ok(match &item.wrapped_key {
            Some(wrapped) => ItemKey::Own(scope_key.unwrap_key(wrapped)?),
            None => ItemKey::Scope(scope_key),
        })
    }

    /// Downloads and decrypts everything in `scope`.
    pub async fn load(&self, scope: &Scope) -> Result<Vault> {
        let sync = self.session.get("/sync?excludeDomains=true").await?;
        let scope_key = self.scope_key(scope)?;
        let list = |name: &str| {
            sync.get(name)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };

        let mut vault = Vault::default();
        match scope {
            Scope::Personal => {
                for folder in list("folders") {
                    vault.folders.push(Folder {
                        id: required_str(&folder, "id", "folder")?,
                        name: scope_key
                            .decrypt_str(&required_str(&folder, "name", "folder")?)
                            .context("decrypting a folder name")?,
                    });
                }
            }
            Scope::Organization(org) => {
                for collection in list("collections") {
                    if collection.get("organizationId").and_then(Value::as_str) != Some(org) {
                        continue;
                    }
                    vault.collections.push(Collection {
                        id: required_str(&collection, "id", "collection")?,
                        name: scope_key
                            .decrypt_str(&required_str(&collection, "name", "collection")?)
                            .context("decrypting a collection name")?,
                        external_id: collection
                            .get("externalId")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_string),
                    });
                }
            }
        }
        for cipher in list("ciphers") {
            let owner = cipher.get("organizationId").and_then(Value::as_str);
            if owner != scope.organization_id() {
                continue;
            }
            let id = required_str(&cipher, "id", "item")?;
            let item = decrypt_item(&cipher, scope_key)
                .with_context(|| format!("decrypting item {id}"))?;
            vault.items.push(item);
        }
        Ok(vault)
    }

    pub async fn create_folder(&self, name: &str) -> Result<Folder> {
        let body = json!({ "name": self.keys.user.encrypt_str(name)? });
        let created = self
            .session
            .send_once(Method::POST, "/folders", Some(&body))
            .await?;
        Ok(Folder {
            id: required_str(&created, "id", "created folder")?,
            name: name.to_string(),
        })
    }

    pub async fn delete_folder(&self, id: &str) -> Result<()> {
        self.delete(&format!("/folders/{id}")).await
    }

    pub async fn create_collection(
        &self,
        org: &str,
        name: &str,
        external_id: Option<&str>,
    ) -> Result<Collection> {
        let key = self.scope_key(&Scope::Organization(org.to_string()))?;
        let body = json!({
            "name": key.encrypt_str(name)?,
            "externalId": external_id,
            "groups": [],
            // Grant the creating account access, or it could not see what it mirrors.
            "users": [],
        });
        let path = format!("/organizations/{org}/collections");
        let created = self
            .session
            .send_once(Method::POST, &path, Some(&body))
            .await?;
        Ok(Collection {
            id: required_str(&created, "id", "created collection")?,
            name: name.to_string(),
            external_id: external_id.map(str::to_string),
        })
    }

    pub async fn delete_collection(&self, org: &str, id: &str) -> Result<()> {
        self.delete(&format!("/organizations/{org}/collections/{id}"))
            .await
    }

    pub async fn create_item(&self, scope: &Scope, draft: &Draft) -> Result<Item> {
        let key = self.scope_key(scope)?;
        let cipher = self.cipher_body(scope, draft, key, None)?;
        let created = match scope {
            Scope::Personal => {
                self.session
                    .send_once(Method::POST, "/ciphers", Some(&cipher))
                    .await?
            }
            Scope::Organization(_) => {
                let body = json!({ "cipher": cipher, "collectionIds": draft.collection_ids });
                self.session
                    .send_once(Method::POST, "/ciphers/create", Some(&body))
                    .await?
            }
        };
        let mut item = decrypt_item(&created, key).context("reading the created item")?;
        if item.collection_ids.is_empty() {
            item.collection_ids = draft.collection_ids.clone();
        }
        Ok(item)
    }

    /// Replaces the content of `existing`. Fails if the item changed on the
    /// server since it was loaded.
    pub async fn update_item(&self, scope: &Scope, existing: &Item, draft: &Draft) -> Result<Item> {
        let scope_key = self.scope_key(scope)?;
        let item_key = self.item_key(scope, existing)?;
        let mut cipher = self.cipher_body(
            scope,
            draft,
            item_key.get(),
            existing.wrapped_key.as_deref(),
        )?;
        cipher["lastKnownRevisionDate"] = json!(existing.revision_date);
        let path = format!("/ciphers/{}", existing.id);
        let updated = self
            .session
            .send_idempotent(Method::PUT, &path, Some(&cipher))
            .await?;
        if scope.organization_id().is_some()
            && sorted(&existing.collection_ids) != sorted(&draft.collection_ids)
        {
            let body = json!({ "collectionIds": draft.collection_ids });
            let path = format!("/ciphers/{}/collections", existing.id);
            self.session
                .send_idempotent(Method::PUT, &path, Some(&body))
                .await?;
        }
        let mut item = decrypt_item(&updated, scope_key).context("reading the updated item")?;
        item.collection_ids = draft.collection_ids.clone();
        Ok(item)
    }

    fn cipher_body(
        &self,
        scope: &Scope,
        draft: &Draft,
        key: &SymKey,
        wrapped_key: Option<&str>,
    ) -> Result<Value> {
        let mut body = Map::new();
        for (name, node) in &draft.content {
            if !is_content_key(name, node) {
                bail!("draft contains a non-content field: {name}");
            }
            body.insert(name.clone(), node.encrypt(key)?);
        }
        body.insert("folderId".into(), json!(draft.folder_id));
        body.insert("organizationId".into(), json!(scope.organization_id()));
        body.insert("key".into(), json!(wrapped_key));
        body.insert("encryptedFor".into(), json!(self.user_id));
        Ok(Value::Object(body))
    }

    /// Permanently deletes items, bypassing the trash.
    pub async fn delete_items(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(BULK_LIMIT) {
            let body = json!({ "ids": chunk });
            self.session
                .send_idempotent(Method::DELETE, "/ciphers", Some(&body))
                .await?;
        }
        Ok(())
    }

    /// Moves items to the trash, as a user deleting them in a client does.
    pub async fn trash_items(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(BULK_LIMIT) {
            let body = json!({ "ids": chunk });
            self.session
                .send_idempotent(Method::PUT, "/ciphers/delete", Some(&body))
                .await?;
        }
        Ok(())
    }

    pub async fn set_archived(&self, ids: &[String], archived: bool) -> Result<()> {
        let path = if archived {
            "/ciphers/archive"
        } else {
            "/ciphers/unarchive"
        };
        for chunk in ids.chunks(BULK_LIMIT) {
            let body = json!({ "ids": chunk });
            self.session
                .send_idempotent(Method::PUT, path, Some(&body))
                .await?;
        }
        Ok(())
    }

    pub async fn download_attachment(
        &self,
        scope: &Scope,
        item: &Item,
        attachment: &Attachment,
    ) -> Result<Vec<u8>> {
        let path = format!("/ciphers/{}/attachment/{}", item.id, attachment.id);
        let details = self.session.get(&path).await?;
        let url = required_str(&details, "url", "attachment")?;
        let blob = self.session.download(&url).await?;
        let item_key = self.item_key(scope, item)?;
        let data = match &attachment.wrapped_key {
            Some(wrapped) => item_key.get().unwrap_key(wrapped)?.decrypt_blob(&blob)?,
            None => item_key.get().decrypt_blob(&blob)?,
        };
        Ok(data)
    }

    pub async fn upload_attachment(
        &self,
        scope: &Scope,
        item: &Item,
        file_name: &str,
        data: &[u8],
    ) -> Result<()> {
        let item_key = self.item_key(scope, item)?;
        let attachment_key = SymKey::generate();
        let blob = attachment_key.encrypt_blob(data)?;
        let encrypted_name = item_key.get().encrypt_str(file_name)?;
        let body = json!({
            "key": item_key.get().wrap_key(&attachment_key)?,
            "fileName": encrypted_name,
            "fileSize": blob.len(),
            "adminRequest": false,
        });
        let path = format!("/ciphers/{}/attachment/v2", item.id);
        let slot = self
            .session
            .send_once(Method::POST, &path, Some(&body))
            .await?;
        let attachment_id = required_str(&slot, "attachmentId", "attachment upload")?;
        let uploaded = match slot.get("fileUploadType").and_then(Value::as_u64) {
            Some(1) => {
                let url = required_str(&slot, "url", "attachment upload")?;
                self.session.upload_azure(&url, blob).await
            }
            _ => {
                let path = format!("/ciphers/{}/attachment/{attachment_id}", item.id);
                self.session
                    .upload_direct(&path, &encrypted_name, blob)
                    .await
            }
        };
        if let Err(error) = uploaded {
            // Do not leave a registered attachment without a file behind.
            let _ = self.delete_attachment(&item.id, &attachment_id).await;
            return Err(error.into());
        }
        Ok(())
    }

    pub async fn delete_attachment(&self, item_id: &str, attachment_id: &str) -> Result<()> {
        self.delete(&format!("/ciphers/{item_id}/attachment/{attachment_id}"))
            .await
    }

    /// Deletes one object, treating "already gone" as success so a retried
    /// or repeated delete converges.
    async fn delete(&self, path: &str) -> Result<()> {
        match self
            .session
            .send_idempotent(Method::DELETE, path, None)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.status() == Some(reqwest::StatusCode::NOT_FOUND) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

enum ItemKey<'a> {
    Scope(&'a SymKey),
    Own(SymKey),
}

impl ItemKey<'_> {
    fn get(&self) -> &SymKey {
        match self {
            Self::Scope(key) => key,
            Self::Own(key) => key,
        }
    }
}

fn sorted(ids: &[String]) -> Vec<&String> {
    let mut ids: Vec<&String> = ids.iter().collect();
    ids.sort();
    ids
}

/// Decides whether a top-level field of a server item is mirrored content.
///
/// Known scalar and list fields are content. So is every object, because that
/// is where each item type keeps its data, including types that do not exist
/// yet. Unknown scalars are left out: servers add per-account flags there.
pub fn is_content_key(name: &str, node: &Node) -> bool {
    if META_KEYS.contains(&name) {
        return false;
    }
    SCALAR_CONTENT_KEYS.contains(&name)
        || LIST_CONTENT_KEYS.contains(&name)
        || matches!(node, Node::Map(_))
}

fn required_str(value: &Value, name: &str, what: &str) -> Result<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{what} has no {name}"))
}

fn optional_str(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn decrypt_item(cipher: &Value, scope_key: &SymKey) -> Result<Item> {
    let object = cipher
        .as_object()
        .ok_or_else(|| anyhow!("item is not an object"))?;
    let wrapped_key = optional_str(cipher, "key");
    let own_key = wrapped_key
        .as_deref()
        .map(|wrapped| scope_key.unwrap_key(wrapped))
        .transpose()
        .context("unwrapping the item key")?;
    let key = own_key.as_ref().unwrap_or(scope_key);

    let mut content = BTreeMap::new();
    for (name, value) in object {
        if META_KEYS.contains(&name.as_str()) {
            continue;
        }
        let node = Node::decrypt(value, key).with_context(|| format!("field {name}"))?;
        if is_content_key(name, &node) {
            content.insert(name.clone(), node);
        }
    }

    let mut attachments = Vec::new();
    for attachment in cipher
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let size = match attachment.get("size") {
            Some(Value::String(text)) => text.parse().ok(),
            Some(Value::Number(number)) => number.as_u64(),
            _ => None,
        };
        attachments.push(Attachment {
            id: required_str(attachment, "id", "attachment")?,
            file_name: key
                .decrypt_str(&required_str(attachment, "fileName", "attachment")?)
                .context("decrypting an attachment name")?,
            size: size.ok_or_else(|| anyhow!("attachment has no valid size"))?,
            wrapped_key: optional_str(attachment, "key"),
        });
    }

    let collection_ids = cipher
        .get("collectionIds")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Ok(Item {
        id: required_str(cipher, "id", "item")?,
        folder_id: optional_str(cipher, "folderId"),
        collection_ids,
        revision_date: optional_str(cipher, "revisionDate"),
        creation_date: optional_str(cipher, "creationDate"),
        in_trash: optional_str(cipher, "deletedDate").is_some(),
        archived: optional_str(cipher, "archivedDate").is_some(),
        archived_date: optional_str(cipher, "archivedDate"),
        content,
        attachments,
        wrapped_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_decrypt_into_content_and_metadata() {
        let key = SymKey::generate();
        let enc = |text: &str| key.encrypt_str(text).unwrap();
        let cipher = json!({
            "object": "cipherDetails",
            "id": "item-1",
            "organizationId": null,
            "folderId": "folder-1",
            "collectionIds": [],
            "type": 1,
            "name": enc("GitHub"),
            "notes": null,
            "favorite": true,
            "reprompt": 0,
            "edit": true,
            "viewPassword": true,
            "permissions": {"delete": true, "restore": true},
            "organizationUseTotp": false,
            "futureServerFlag": true,
            "revisionDate": "2026-01-02T03:04:05.000000Z",
            "creationDate": "2026-01-01T00:00:00.000000Z",
            "deletedDate": null,
            "archivedDate": "2026-01-03T00:00:00.000000Z",
            "key": null,
            "login": {"username": enc("octocat"), "password": enc("pw"), "totp": null},
            "data": {"username": enc("octocat")},
            "futureItemType": {"number": enc("42")},
            "fields": [{"name": enc("f"), "value": enc("v"), "type": 0, "linkedId": null}],
            "passwordHistory": null,
            "attachments": [{"id": "att-1", "fileName": enc("a.txt"), "size": "12", "key": null}],
        });
        let item = decrypt_item(&cipher, &key).unwrap();

        assert_eq!(item.id, "item-1");
        assert_eq!(item.folder_id.as_deref(), Some("folder-1"));
        assert!(item.archived);
        assert!(!item.in_trash);
        assert_eq!(item.name(), "GitHub");
        assert_eq!(item.attachments.len(), 1);
        assert_eq!(item.attachments[0].file_name, "a.txt");
        assert_eq!(item.attachments[0].size, 12);
        let mut keys: Vec<&str> = item.content.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "favorite",
                "fields",
                "futureItemType",
                "login",
                "name",
                "notes",
                "passwordHistory",
                "reprompt",
                "type"
            ]
        );
    }

    #[test]
    fn an_item_with_its_own_key_is_decrypted_with_that_key() {
        let user_key = SymKey::generate();
        let item_key = SymKey::generate();
        let cipher = json!({
            "id": "item-1",
            "type": 2,
            "name": item_key.encrypt_str("Note").unwrap(),
            "key": user_key.wrap_key(&item_key).unwrap(),
            "attachments": [{"id": "a", "fileName": item_key.encrypt_str("f").unwrap(), "size": 1}],
        });
        let item = decrypt_item(&cipher, &user_key).unwrap();
        assert_eq!(item.name(), "Note");
        assert_eq!(item.attachments[0].file_name, "f");
        assert!(item.wrapped_key.is_some());
        assert!(decrypt_item(&cipher, &SymKey::generate()).is_err());
    }
}
