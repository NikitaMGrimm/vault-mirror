//! A decrypted view of one account's vault, and the operations that change it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::{
    api::{self, ApiCredentials, ServerUrls, Session},
    crypto::{KdfParams, MasterSecret, SymKey},
    node::Node,
    text::count,
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

/// How many item writes run at once.
pub const WRITES_AT_ONCE: usize = 4;

/// Up to this many new items are created one by one, which avoids the
/// extra sync an import needs.
const SINGLE_CREATE_LIMIT: usize = 10;

/// Bank account, driver's licence and passport.
const FLAGGED_ITEM_TYPES: [u64; 3] = [6, 7, 8];

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
    pub fn item_type(&self) -> Option<u64> {
        match self.content.get("type") {
            Some(Node::Number(number)) => number.as_u64(),
            _ => None,
        }
    }

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
    /// Personal folders that organization items are in. Folders belong to
    /// the account, so a personal run has to see what else uses them.
    pub folders_used_elsewhere: BTreeSet<String>,
}

struct Keys {
    user: SymKey,
    organizations: HashMap<String, SymKey>,
}

/// Membership ids by organization id.
type Memberships = HashMap<String, String>;

/// One unlocked account on one server.
pub struct Account {
    session: Session,
    keys: Keys,
    memberships: Memberships,
    pub user_id: String,
    /// Whether the account may upload attachments to its individual vault.
    /// Bitwarden requires a premium subscription; Vaultwarden reports every
    /// account as premium.
    pub premium: bool,
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
        let flag = |name: &str| profile.get(name).and_then(Value::as_bool);
        let premium =
            flag("premium").unwrap_or(true) || flag("premiumFromOrganization") == Some(true);

        let kdf = match session.login_kdf {
            Some(kdf) => Some(kdf),
            None => api::prelogin(&http, &urls, &email).await?,
        };
        let (kind, iterations, memory, parallelism) =
            kdf.ok_or_else(|| anyhow!("the server did not report key derivation settings"))?;
        let kdf = KdfParams::from_server(kind, iterations, memory, parallelism)?;

        // Key derivation is deliberately expensive; keep it off the async workers.
        let password = master_password.to_string();
        let salt = email;
        let master =
            tokio::task::spawn_blocking(move || MasterSecret::derive(&password, &salt, &kdf))
                .await
                .context("key derivation task failed")??;
        let user = master.unlock_user_key(&text("key")?)?;

        let mut organizations = HashMap::new();
        let mut member_ids = Memberships::new();
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
                if let Some(member) = membership.get("organizationUserId").and_then(Value::as_str) {
                    member_ids.insert(id.to_string(), member.to_string());
                }
            }
        }

        Ok(Self {
            session,
            keys: Keys {
                user,
                organizations,
            },
            memberships: member_ids,
            user_id,
            premium,
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
        let list = |name: &str| sync_list(&sync, name);

        let mut vault = Vault::default();
        match scope {
            Scope::Personal => {
                for folder in list("folders")? {
                    vault.folders.push(Folder {
                        id: required_str(&folder, "id", "folder")?,
                        name: scope_key
                            .decrypt_str(&required_str(&folder, "name", "folder")?)
                            .context("decrypting a folder name")?,
                    });
                }
            }
            Scope::Organization(org) => {
                let collections = list("collections")?;
                self.check_organization_visible(org, &collections, &list("ciphers")?)
                    .await?;
                for collection in collections {
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
        for cipher in list("ciphers")? {
            let owner = cipher.get("organizationId").and_then(Value::as_str);
            if owner != scope.organization_id() {
                if *scope == Scope::Personal {
                    vault
                        .folders_used_elsewhere
                        .extend(optional_str(&cipher, "folderId"));
                }
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
        // Bitwarden's server shows owners only the collections they are
        // assigned to, so the creating account assigns itself, as Bitwarden's
        // clients do. Vaultwarden shows owners every collection either way.
        let users = match self.memberships.get(org) {
            Some(member) => json!([{
                "id": member,
                "readOnly": false,
                "hidePasswords": false,
                "manage": true,
            }]),
            None => json!([]),
        };
        let body = json!({
            "name": key.encrypt_str(name)?,
            "externalId": external_id,
            "groups": [],
            "users": users,
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

    /// Creates items, returned in the order of `drafts`. In an individual
    /// vault, more than a few items are imported in bulk: Bitwarden's cloud
    /// accepts only about 60 single creations a minute. `folder_names` maps
    /// the drafts' folder ids to names, which an import must repeat.
    pub async fn create_items(
        &self,
        scope: &Scope,
        drafts: &[Draft],
        folder_names: &HashMap<String, String>,
        created: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<Item>> {
        if *scope != Scope::Personal || drafts.len() <= SINGLE_CREATE_LIMIT {
            return stream::iter(drafts.iter().map(|draft| self.create_item(scope, draft)))
                .buffered(WRITES_AT_ONCE)
                .inspect_ok(|_| created(1))
                .try_collect()
                .await;
        }
        let key = &self.keys.user;
        let mut sent_names = Vec::with_capacity(drafts.len());
        for chunk in drafts.chunks(BULK_LIMIT) {
            let mut ciphers = Vec::with_capacity(chunk.len());
            let mut folders: Vec<Value> = Vec::new();
            let mut folder_positions: HashMap<&str, usize> = HashMap::new();
            let mut relationships = Vec::new();
            for (index, draft) in chunk.iter().enumerate() {
                let cipher = self.cipher_body(scope, draft, key, None)?;
                sent_names.push(required_str(&cipher, "name", "item to import")?);
                ciphers.push(cipher);
                let Some(folder) = draft.folder_id.as_deref() else {
                    continue;
                };
                let position = match folder_positions.get(folder) {
                    Some(&position) => position,
                    None => {
                        let name = folder_names
                            .get(folder)
                            .ok_or_else(|| anyhow!("no name for folder {folder}"))?;
                        // An existing folder id is kept; the name is required
                        // all the same.
                        folders.push(json!({ "id": folder, "name": key.encrypt_str(name)? }));
                        folder_positions.insert(folder, folders.len() - 1);
                        folders.len() - 1
                    }
                };
                relationships.push(json!({ "key": index, "value": position }));
            }
            let body = json!({
                "ciphers": ciphers,
                "folders": folders,
                "folderRelationships": relationships,
            });
            self.session
                .send_once(Method::POST, "/ciphers/import", Some(&body))
                .await?;
            created(chunk.len());
        }

        // An import does not report the new ids. Every encryption is
        // randomised and servers store ciphertext as sent, so each new item
        // is found by its encrypted name.
        let sync = self.session.get("/sync?excludeDomains=true").await?;
        let by_name: HashMap<&str, &Value> = sync
            .get("ciphers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|cipher| cipher.get("organizationId").is_none_or(Value::is_null))
            .filter_map(|cipher| Some((cipher.get("name")?.as_str()?, cipher)))
            .collect();
        sent_names
            .iter()
            .map(|name| {
                let cipher = by_name.get(name.as_str()).ok_or_else(|| {
                    anyhow!("an imported item is missing from the vault after the import")
                })?;
                decrypt_item(cipher, key).context("reading an imported item")
            })
            .collect()
    }

    /// Reads one item as the server now holds it. Collections are left
    /// empty: not every server reports them here.
    pub async fn get_item(&self, scope: &Scope, id: &str) -> Result<Item> {
        let scope_key = self.scope_key(scope)?;
        let cipher = self.session.get(&format!("/ciphers/{id}")).await?;
        decrypt_item(&cipher, scope_key).context("reading the item")
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
        // A replacement that leaves this out un-archives the item.
        cipher["archivedDate"] = json!(existing.archived_date);
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
        if existing.archived {
            // Bitwarden's server on Postgres, MySQL or SQLite answers with the
            // archive date it was sent but stores the item unarchived. Its
            // archive endpoint rejects items that are still archived, so the
            // stored state is read back first.
            let stored = self.session.get(&path).await?;
            if optional_str(&stored, "archivedDate").is_none() {
                self.set_archived(std::slice::from_ref(&existing.id), true)
                    .await?;
            }
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

    async fn check_organization_visible(
        &self,
        org: &str,
        synced_collections: &[Value],
        synced_items: &[Value],
    ) -> Result<()> {
        let collections_path = format!("/organizations/{org}/collections");
        let items_path = format!("/ciphers/organization-details?organizationId={org}");
        let (collections, items) = tokio::join!(
            self.session.get(&collections_path),
            self.session.get(&items_path),
        );
        check_organization_visible(
            org,
            collections.map_err(Into::into),
            items.map_err(Into::into),
            synced_collections,
            synced_items,
        )
    }

    /// What the server accepts and shows, as far as mirroring depends on it.
    pub async fn server_traits(&self) -> Result<ServerTraits> {
        Ok(server_traits(&self.session.get("/config").await?))
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

    /// Adds a file to `item` and returns the new attachment's id. A failed
    /// upload leaves the attachment the server registered for it in place,
    /// since the file may have arrived all the same.
    pub async fn upload_attachment(
        &self,
        scope: &Scope,
        item: &Item,
        file_name: &str,
        data: &[u8],
    ) -> Result<String> {
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
        match slot.get("fileUploadType").and_then(Value::as_u64) {
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
        }
        .with_context(|| format!("uploading attachment {attachment_id} of item {}", item.id))?;
        Ok(attachment_id)
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

/// A list from a sync response. A missing list must not read as an empty
/// vault, which would have every mirrored item deleted.
fn sync_list(sync: &Value, name: &str) -> Result<Vec<Value>> {
    sync.get(name)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("the server's sync response has no {name} list"))
}

/// Fails when the organization holds collections or items this account
/// cannot see. Bitwarden's server shows an owner only the collections the
/// owner is assigned to, and no item that is in no collection. Hidden items
/// would look deleted, and their copies would be deleted.
fn check_organization_visible(
    org: &str,
    all_collections: Result<Value>,
    all_items: Result<Value>,
    synced_collections: &[Value],
    synced_items: &[Value],
) -> Result<()> {
    // Only owners and admins may list everything. Without the lists, items
    // hidden from this account could not be told from deleted ones.
    let context = || {
        format!(
            "listing everything in organization {org}; the account must be an owner or admin of it"
        )
    };
    let (all_collections, all_items) = (
        all_collections.with_context(context)?,
        all_items.with_context(context)?,
    );
    let hidden = |name: &str, all: &Value, synced: &[Value]| -> Result<usize> {
        let all = all.get("data").and_then(Value::as_array).ok_or_else(|| {
            anyhow!("the server's list of all {name} in organization {org} is malformed")
        })?;
        let visible: HashSet<&str> = synced
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .collect();
        Ok(all
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .filter(|id| !visible.contains(id))
            .count())
    };
    let collections = hidden("collections", &all_collections, synced_collections)?;
    let items = hidden("items", &all_items, synced_items)?;
    if collections + items > 0 {
        bail!(
            "organization {org} holds {} and {} this account cannot see; assign the account \
             to every collection with Can manage permission, and put every item in a collection",
            count(collections, "collection"),
            count(items, "item")
        );
    }
    Ok(())
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

pub struct ServerTraits {
    /// Item types the server accepts but leaves out of syncs. Copies of such
    /// items would never be seen and would be created again every run.
    pub hidden_item_types: Vec<u64>,
    /// Whether an organization item must be in at least one collection.
    pub items_need_collections: bool,
}

impl ServerTraits {
    pub fn hides(&self, item: &Item) -> bool {
        item.item_type()
            .is_some_and(|kind| self.hidden_item_types.contains(&kind))
    }
}

fn server_traits(config: &Value) -> ServerTraits {
    let vaultwarden = config.pointer("/server/name").and_then(Value::as_str) == Some("Vaultwarden");
    // Bitwarden's own server shows bank accounts, driver's licences and
    // passports only where a feature flag is on. Vaultwarden always does.
    let flagged_types_shown = config
        .pointer("/featureStates/pm-32009-new-item-types")
        .and_then(Value::as_bool)
        == Some(true);
    ServerTraits {
        hidden_item_types: if vaultwarden || flagged_types_shown {
            Vec::new()
        } else {
            FLAGGED_ITEM_TYPES.to_vec()
        },
        items_need_collections: !vaultwarden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_response_without_a_list_is_an_error_not_an_empty_vault() {
        let sync = json!({ "ciphers": [], "folders": null });
        assert!(sync_list(&sync, "ciphers").unwrap().is_empty());
        assert!(sync_list(&sync, "folders").is_err());
        assert!(sync_list(&sync, "collections").is_err());
    }

    #[test]
    fn an_organization_this_account_cannot_list_fully_is_refused() {
        let synced = [json!({ "id": "c1" })];
        let all = || Ok(json!({ "data": [{ "id": "c1" }] }));
        let check = |collections, items| {
            check_organization_visible("org", collections, items, &synced, &synced)
        };
        assert!(check(all(), all()).is_ok());
        let refused = check(Err(anyhow!("403 Forbidden")), all()).unwrap_err();
        assert!(format!("{refused:#}").contains("owner or admin"));
        assert!(check(all(), Err(anyhow!("404 Not Found"))).is_err());
        assert!(check(all(), Ok(json!({}))).is_err());
        let hidden = check(
            all(),
            Ok(json!({ "data": [{ "id": "c1" }, { "id": "i2" }] })),
        );
        assert!(format!("{:#}", hidden.unwrap_err()).contains("1 item"));
    }

    #[test]
    fn server_traits_tell_bitwarden_from_vaultwarden() {
        let flagged = FLAGGED_ITEM_TYPES.to_vec();
        // Bitwarden's cloud and self-hosted server send no server name.
        let bitwarden = |flags| server_traits(&json!({"server": null, "featureStates": flags}));
        assert_eq!(
            bitwarden(json!({"pm-32009-new-item-types": false})).hidden_item_types,
            flagged
        );
        assert_eq!(bitwarden(json!({})).hidden_item_types, flagged);
        let enabled = bitwarden(json!({"pm-32009-new-item-types": true}));
        assert!(enabled.hidden_item_types.is_empty());
        assert!(enabled.items_need_collections);

        let vaultwarden = server_traits(&json!({
            "server": {"name": "Vaultwarden", "url": "https://github.com/dani-garcia/vaultwarden"},
            "featureStates": {}
        }));
        assert!(vaultwarden.hidden_item_types.is_empty());
        assert!(!vaultwarden.items_need_collections);
    }

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

    fn full_cipher(key: &SymKey) -> Value {
        let enc = |text: &str| json!(key.encrypt_str(text).unwrap());
        json!({
            "id": "item-1",
            "type": 1,
            "name": enc("2.AAAA"),
            "notes": enc("2.5"),
            "revisionDate": "2026-01-02T03:04:05.000000Z",
            "futureVersion": "2.5",
            "futureItemType": {"version": "2.AAAA", "date": "2026-01-02"},
            "login": {
                "username": enc("u"), "password": enc("p"), "totp": enc("t"),
                "uri": enc("https://x"),
                "passwordRevisionDate": "2026-03-04T05:06:07.000Z",
                "uris": [{"uri": enc("https://x"), "uriChecksum": enc("c"), "match": null}],
                "fido2Credentials": [{
                    "credentialId": enc("id"), "keyType": enc("public-key"),
                    "keyAlgorithm": enc("ECDSA"), "keyCurve": enc("P-256"),
                    "keyValue": enc("k"), "rpId": enc("x"), "rpName": enc("X"),
                    "userHandle": enc("h"), "userName": enc("u"),
                    "userDisplayName": enc("U"), "counter": enc("0"),
                    "discoverable": enc("true"),
                    "creationDate": "2026-02-03T04:05:06.000Z",
                }],
            },
            "card": {
                "cardholderName": enc("J"), "brand": enc("Visa"), "number": enc("4111"),
                "expMonth": enc("12"), "expYear": enc("2031"), "code": enc("123"),
            },
            "identity": {"firstName": enc("Jane"), "passportNumber": enc("X1"), "ssn": ""},
            "sshKey": {"privateKey": enc("k"), "publicKey": enc("p"), "keyFingerprint": enc("f")},
            "bankAccount": {"iban": enc("DE02"), "pin": enc("1234"), "swiftCode": null},
            "driversLicense": {"licenseNumber": enc("B07"), "issueDate": enc("2015-03-01")},
            "passport": {"surname": enc("M"), "dateOfBirth": enc("1964-08-12"), "expirationDate": ""},
            "fields": [
                {"name": enc("f"), "value": enc("v"), "type": 0, "linkedId": null},
                {"name": enc("linked"), "value": null, "type": 3, "linkedId": 100},
            ],
            "passwordHistory": [{"password": enc("old"), "lastUsedDate": "2025-01-02T03:04:05.000Z"}],
        })
    }

    #[test]
    fn known_encrypted_slots_accept_ciphertext_and_absent_values() {
        let key = SymKey::generate();
        let cipher = full_cipher(&key);
        let item = decrypt_item(&cipher, &key).unwrap();
        assert_eq!(item.name(), "2.AAAA");
        assert_eq!(
            item.revision_date.as_deref(),
            Some("2026-01-02T03:04:05.000000Z")
        );
        assert_eq!(
            item.content["login"].as_map().unwrap()["passwordRevisionDate"],
            Node::Text("2026-03-04T05:06:07.000Z".into())
        );
        assert_eq!(
            item.content["futureItemType"].as_map().unwrap()["version"],
            Node::Text("2.AAAA".into())
        );

        for (pointer, value) in [
            ("/notes", Value::Null),
            ("/login/password", Value::Null),
            ("/login/totp", json!("")),
            ("/login/uris/0/uriChecksum", Value::Null),
            ("/fields/0/value", json!("")),
            ("/passwordHistory/0/password", Value::Null),
            ("/card", Value::Null),
            ("/passwordHistory", Value::Null),
            ("/login/uris", json!([])),
        ] {
            let mut cipher = full_cipher(&key);
            *cipher.pointer_mut(pointer).unwrap() = value;
            decrypt_item(&cipher, &key).unwrap_or_else(|error| panic!("{pointer}: {error:#}"));
        }
        let mut missing = full_cipher(&key);
        let object = missing.as_object_mut().unwrap();
        for name in ["notes", "login", "fields", "passwordHistory"] {
            object.remove(name);
        }
        decrypt_item(&missing, &key).unwrap();
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
