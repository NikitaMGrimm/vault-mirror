//! A decrypted view of one account's vault, and the operations that change it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::Method;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

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
    // A legacy duplicate of the content, as an object or as a string that
    // holds one. Any other string is an item this tool cannot read.
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

/// Content that is encrypted at rest. A string there is ciphertext or an
/// error, whatever it looks like. `*` stands for any key, or, followed by
/// more parts, any entry of a list of objects. Other content strings are told
/// apart by their shape.
const ENCRYPTED_SLOTS: &[&str] = &[
    "name",
    "notes",
    "login/username",
    "login/password",
    "login/totp",
    "login/uri",
    "login/uris/*/uri",
    "login/uris/*/uriChecksum",
    "login/fido2Credentials/*/credentialId",
    "login/fido2Credentials/*/keyType",
    "login/fido2Credentials/*/keyAlgorithm",
    "login/fido2Credentials/*/keyCurve",
    "login/fido2Credentials/*/keyValue",
    "login/fido2Credentials/*/rpId",
    "login/fido2Credentials/*/rpName",
    "login/fido2Credentials/*/userHandle",
    "login/fido2Credentials/*/userName",
    "login/fido2Credentials/*/userDisplayName",
    "login/fido2Credentials/*/counter",
    "login/fido2Credentials/*/discoverable",
    "fields/*/name",
    "fields/*/value",
    "passwordHistory/*/password",
    // These item types hold nothing but encrypted strings.
    "card/*",
    "identity/*",
    "sshKey/*",
    "bankAccount/*",
    "driversLicense/*",
    "passport/*",
];

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
    /// SHA-256 of the encrypted server name. Reencrypting a name changes
    /// this value even when its plaintext stays the same.
    pub(crate) stamp: Option<String>,
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

/// A vault read with matching item visibility observations before and after it.
pub struct VaultRead {
    vault: Vault,
    traits: ServerTraits,
}

impl VaultRead {
    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    pub fn traits(&self) -> &ServerTraits {
        &self.traits
    }
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
        if let Scope::Organization(org) = scope {
            let list = |name: &str| sync_list(&sync, name);
            self.check_organization_visible(org, &list("collections")?, &list("ciphers")?)
                .await?;
        }
        read_vault(&sync, scope, scope_key)
    }

    /// Reads a vault for backup or planning, retaining its visibility evidence.
    pub async fn load_with_traits(&self, scope: &Scope) -> Result<VaultRead> {
        let traits = self
            .server_traits()
            .await
            .context("reading the server's settings before the vault")?;
        let vault = self.load(scope).await?;
        let after = self
            .server_traits()
            .await
            .context("reading the server's settings after the vault")?;
        if traits.hidden_item_types != after.hidden_item_types {
            bail!(
                "the server's item visibility changed while reading the vault. Run again \
                 when its settings are stable"
            );
        }
        Ok(VaultRead { vault, traits })
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
            stamp: optional_str(&created, "name")?
                .as_deref()
                .map(encrypted_name_hash),
        })
    }

    /// Deletes a folder. Returns false if it was already gone. Not retried
    /// once the request may have arrived: a folder has no revision, so a
    /// repeat could delete one that was meanwhile changed.
    pub async fn delete_folder(&self, id: &str) -> Result<bool> {
        match (self.session)
            .send_once(Method::DELETE, &format!("/folders/{id}"), None)
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if error.status() == Some(reqwest::StatusCode::NOT_FOUND) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Reads the personal vault for folder cleanup. Requires an explicitly empty
    /// organization list, no known hidden item types, unique folder IDs, and
    /// well-formed folder references.
    pub(crate) async fn load_folder_contents(&self) -> Result<Vault> {
        let (profile, config, sync) = tokio::try_join!(
            self.session.get("/accounts/profile"),
            self.session.get("/config"),
            self.session.get("/sync?excludeDomains=true"),
        )?;
        check_folder_contents(&profile, &server_traits(&config), &sync)?;
        read_vault(&sync, &Scope::Personal, &self.keys.user)
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

    /// Replaces the content of `existing`, sending its last known revision.
    /// The server's revision check does not catch every intervening edit.
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
            if optional_str(&stored, "archivedDate")?.is_none() {
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

/// Rejects known visibility gaps and malformed folder references before
/// parsing. A personal Vault does not preserve malformed references or
/// establish organization visibility.
fn check_folder_contents(profile: &Value, traits: &ServerTraits, sync: &Value) -> Result<()> {
    match profile.get("organizations") {
        Some(Value::Array(list)) if list.is_empty() => {}
        Some(Value::Array(_)) => {
            bail!("the account is in an organization, whose items can be in its folders unseen")
        }
        _ => bail!("the account profile does not list the account's organizations"),
    }
    if !traits.hidden_item_types.is_empty() {
        bail!("the server does not show every item type, so folders may hold items unseen");
    }
    let mut ids = HashSet::new();
    for folder in sync_list(sync, "folders")? {
        let id = required_str(&folder, "id", "folder")?;
        if !ids.insert(id.clone()) {
            bail!("the server lists folder {id} more than once");
        }
    }
    for cipher in sync_list(sync, "ciphers")? {
        match cipher.get("folderId") {
            None | Some(Value::Null) => {}
            Some(Value::String(id)) if !id.is_empty() => {}
            Some(_) => bail!("an item has a malformed folderId"),
        }
    }
    Ok(())
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
        let malformed =
            || anyhow!("the server's list of all {name} in organization {org} is malformed");
        let all = all
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(malformed)?;
        // Only what `read_vault` keeps is visible.
        let mut visible = HashSet::new();
        for entry in synced {
            if organization_of(entry, &format!("an entry of the synced {name}"))? == Some(org) {
                visible.extend(entry.get("id").and_then(Value::as_str));
            }
        }
        let mut hidden = 0;
        for entry in all {
            let id = (entry.get("id").and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .ok_or_else(malformed)?;
            hidden += usize::from(!visible.contains(id));
        }
        Ok(hidden)
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

fn encrypted_name_hash(encrypted: &str) -> String {
    hex::encode(Sha256::digest(encrypted.as_bytes()))
}

fn optional_str(value: &Value, name: &str) -> Result<Option<String>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => bail!("the server's {name} is not a string"),
    }
}

fn read_vault(sync: &Value, scope: &Scope, scope_key: &SymKey) -> Result<Vault> {
    let list = |name: &str| sync_list(sync, name);
    let mut vault = Vault::default();
    match scope {
        Scope::Personal => {
            for folder in list("folders")? {
                let name = required_str(&folder, "name", "folder")?;
                vault.folders.push(Folder {
                    id: required_str(&folder, "id", "folder")?,
                    name: (scope_key.decrypt_str(&name)).context("decrypting a folder name")?,
                    stamp: Some(encrypted_name_hash(&name)),
                });
            }
        }
        Scope::Organization(org) => {
            for collection in list("collections")? {
                if organization_of(&collection, "a collection")? != Some(org) {
                    continue;
                }
                vault.collections.push(Collection {
                    id: required_str(&collection, "id", "collection")?,
                    name: scope_key
                        .decrypt_str(&required_str(&collection, "name", "collection")?)
                        .context("decrypting a collection name")?,
                    external_id: optional_str(&collection, "externalId")?,
                });
            }
        }
    }
    for cipher in list("ciphers")? {
        let id = required_str(&cipher, "id", "item")?;
        if organization_of(&cipher, &format!("item {id}"))? != scope.organization_id() {
            if *scope == Scope::Personal {
                vault
                    .folders_used_elsewhere
                    .extend(optional_str(&cipher, "folderId")?);
            }
            continue;
        }
        let item =
            decrypt_item(&cipher, scope_key).with_context(|| format!("decrypting item {id}"))?;
        vault.items.push(item);
    }
    Ok(vault)
}

/// The organization an item or collection belongs to. Anything but an id or
/// null is an error: read as no organization, it would move the object to the
/// personal vault or out of its organization.
fn organization_of<'a>(value: &'a Value, what: &str) -> Result<Option<&'a str>> {
    match value.get("organizationId") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if !id.is_empty() => Ok(Some(id)),
        Some(_) => bail!("{what} has a malformed organizationId"),
    }
}

fn is_encrypted_slot(field: &str, pointer: &str) -> bool {
    let at: Vec<&str> = std::iter::once(field)
        .chain(pointer.split('/').skip(1))
        .collect();
    ENCRYPTED_SLOTS.iter().any(|slot| {
        slot.split('/').count() == at.len()
            && slot
                .split('/')
                .zip(&at)
                .all(|(part, segment)| part == "*" || part == *segment)
    })
}

/// The object an item type keeps its data in, such as `login` or `card`.
/// Anything else there that holds content would be left out of the item.
fn is_type_block(name: &str) -> bool {
    // A secure note's block holds only its subtype, which is not encrypted.
    name == "secureNote"
        || ENCRYPTED_SLOTS.iter().any(|slot| {
            slot.split_once('/')
                .is_some_and(|(root, _)| root == name && !LIST_CONTENT_KEYS.contains(&root))
        })
}

/// A list at `name`, a path within an item, that may be missing or null, but
/// nothing else.
fn optional_list<'a>(cipher: &'a Value, name: &str) -> Result<&'a [Value]> {
    match cipher.pointer(&format!("/{name}")) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(list)) => Ok(list),
        Some(_) => bail!("the item's {name} is not a list"),
    }
}

fn decrypt_item(cipher: &Value, scope_key: &SymKey) -> Result<Item> {
    fn contains_secret(node: &Node) -> bool {
        match node {
            Node::Secret(_) => true,
            Node::List(nodes) => nodes.iter().any(contains_secret),
            Node::Map(nodes) => nodes.values().any(contains_secret),
            _ => false,
        }
    }

    let object = cipher
        .as_object()
        .ok_or_else(|| anyhow!("item is not an object"))?;
    // Bitwarden's server sends the legacy duplicate as a string of JSON. It can
    // also store a whole item as one encrypted string in `data`, without the
    // other content fields. Skipping that would read the item as empty.
    if let Some(Value::String(data)) = object.get("data")
        && serde_json::from_str::<Map<String, Value>>(data).is_err()
    {
        bail!("the item is stored as an encrypted data blob, a format that is not supported yet");
    }
    // Content in a list of another shape would be left out of the copy, or
    // skip the checks of its encrypted slots.
    for list in ENCRYPTED_SLOTS
        .iter()
        .filter_map(|slot| slot.split_once("/*/").map(|(list, _)| list))
    {
        if !optional_list(cipher, list)?
            .iter()
            .all(|entry| entry.is_object() || entry.is_null())
        {
            bail!("the item's {list} holds an entry that is not an object");
        }
    }
    let wrapped_key = optional_str(cipher, "key")?;
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
        let node = Node::decrypt(value, key, &|pointer| is_encrypted_slot(name, pointer))
            .with_context(|| format!("field {name}"))?;
        if is_content_key(name, &node) {
            content.insert(name.clone(), node);
        } else if is_type_block(name) && node.normalized().is_some() {
            bail!("field {name} is not an object");
        } else if contains_secret(&node) {
            bail!("the item has encrypted content in a format that is not supported yet");
        }
    }

    let mut attachments = Vec::new();
    for attachment in optional_list(cipher, "attachments")? {
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
            wrapped_key: optional_str(attachment, "key")?,
        });
    }

    let collection_ids = optional_list(cipher, "collectionIds")?
        .iter()
        .map(|id| {
            id.as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("the item's collectionIds hold a value that is not an id"))
        })
        .collect::<Result<_>>()?;

    let archived_date = optional_str(cipher, "archivedDate")?;
    Ok(Item {
        id: required_str(cipher, "id", "item")?,
        folder_id: optional_str(cipher, "folderId")?,
        collection_ids,
        revision_date: optional_str(cipher, "revisionDate")?,
        creation_date: optional_str(cipher, "creationDate")?,
        in_trash: optional_str(cipher, "deletedDate")?.is_some(),
        archived: archived_date.is_some(),
        archived_date,
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
    /// Whether updates preserve organization members' private metadata.
    pub organization_updates_safe: bool,
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
        organization_updates_safe: vaultwarden,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) async fn account(url: &str, user: SymKey) -> Account {
        let http = api::build_http_client(&api::ClientOptions::default()).unwrap();
        let session = Session::login(
            http,
            ServerUrls::from_server(url).unwrap(),
            ApiCredentials {
                client_id: "user.synthetic".into(),
                client_secret: "synthetic secret".into(),
                device_id: "synthetic device".into(),
            },
        )
        .await
        .unwrap();
        Account {
            session,
            keys: Keys {
                user,
                organizations: HashMap::new(),
            },
            memberships: Memberships::new(),
            user_id: "synthetic".into(),
            premium: true,
        }
    }

    impl Account {
        pub(crate) fn test_sync(&self, items: &[Item]) -> Value {
            let ciphers: Vec<_> = items
                .iter()
                .map(|item| {
                    let mut cipher = Node::Map(item.content.clone())
                        .encrypt(&self.keys.user)
                        .unwrap();
                    cipher["id"] = json!(item.id);
                    cipher
                })
                .collect();
            json!({"ciphers": ciphers, "folders": [], "collections": []})
        }
    }

    #[test]
    fn a_sync_response_without_a_list_is_an_error_not_an_empty_vault() {
        let sync = json!({ "ciphers": [], "folders": null });
        assert!(sync_list(&sync, "ciphers").unwrap().is_empty());
        assert!(sync_list(&sync, "folders").is_err());
        assert!(sync_list(&sync, "collections").is_err());
    }

    #[test]
    fn an_organization_this_account_cannot_list_fully_is_refused() {
        let synced = [json!({ "id": "c1", "organizationId": "org" })];
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
    fn only_entries_the_read_keeps_count_as_visible() {
        let key = SymKey::generate();
        let org = Scope::Organization("org-1".into());
        let inventory = |id: &str| Ok(json!({ "data": [{ "id": id }] }));
        for owner in [
            None,
            Some(Value::Null),
            Some(json!("org-2")),
            Some(json!("org-1")),
        ] {
            let mut cipher = full_cipher(&key);
            let mut collection = json!({"id": "c1", "name": key.encrypt_str("Shared").unwrap()});
            if let Some(owner) = &owner {
                cipher["organizationId"] = owner.clone();
                collection["organizationId"] = owner.clone();
            }
            let sync = json!({"collections": [collection], "ciphers": [cipher]});
            let (collections, items) =
                (sync_list(&sync, "collections"), sync_list(&sync, "ciphers"));
            let visible = check_organization_visible(
                "org-1",
                inventory("c1"),
                inventory("item-1"),
                &collections.unwrap(),
                &items.unwrap(),
            );
            let read = read_vault(&sync, &org, &key).unwrap();
            let kept = (read.collections.len(), read.items.len());
            match visible {
                Ok(()) => assert_eq!(kept, (1, 1), "{owner:?}"),
                Err(error) => {
                    assert_eq!(kept, (0, 0), "{owner:?}");
                    let message = format!("{error:#}");
                    assert!(message.contains("1 collection and 1 item"), "{message}");
                }
            }
        }
    }

    #[test]
    fn an_organization_inventory_row_without_an_id_is_an_error() {
        let synced = [json!({ "id": "c1", "organizationId": "org" })];
        for row in [
            json!({}),
            json!({ "id": null }),
            json!({ "id": 7 }),
            json!("c1"),
        ] {
            let all = || Ok(json!({ "data": [{ "id": "c1" }, row.clone()] }));
            let check = |collections, items| {
                check_organization_visible("org", collections, items, &synced, &synced)
            };
            let ok = || Ok(json!({ "data": [{ "id": "c1" }] }));
            for result in [check(all(), ok()), check(ok(), all())] {
                let message = format!("{:#}", result.unwrap_err());
                assert!(message.contains("malformed"), "{message}");
            }
        }
    }

    #[test]
    fn folders_count_as_empty_only_where_the_read_shows_every_item() {
        let shown = server_traits(&json!({"server": {"name": "Vaultwarden"}}));
        let hidden = server_traits(&json!({"server": null, "featureStates": {}}));
        let profile = |organizations: Option<Value>| match organizations {
            Some(list) => json!({"id": "user", "organizations": list}),
            None => json!({"id": "user"}),
        };
        let sync = |folders: Value, folder_id: Value| json!({"folders": folders, "ciphers": [{"id": "item-1", "folderId": folder_id}]});
        let folders = json!([{"id": "f1", "name": "2.x"}, {"id": "f2", "name": "2.y"}]);
        let complete = sync(folders.clone(), json!("f1"));
        let empty = profile(Some(json!([])));
        assert!(check_folder_contents(&empty, &shown, &complete).is_ok());
        assert!(check_folder_contents(&empty, &shown, &sync(folders.clone(), Value::Null)).is_ok());

        let refused = [
            (
                profile(Some(json!([{"id": "org"}]))),
                &shown,
                complete.clone(),
            ),
            (profile(Some(Value::Null)), &shown, complete.clone()),
            (profile(Some(json!({}))), &shown, complete.clone()),
            (profile(None), &shown, complete.clone()),
            (empty.clone(), &hidden, complete.clone()),
            (empty.clone(), &shown, sync(folders.clone(), json!(7))),
            (empty.clone(), &shown, sync(folders.clone(), json!(""))),
            (empty.clone(), &shown, sync(folders.clone(), json!(["f1"]))),
            (
                empty.clone(),
                &shown,
                sync(json!([{"id": "f1"}, {"id": "f1"}]), Value::Null),
            ),
            (
                empty.clone(),
                &shown,
                json!({"folders": folders, "ciphers": null}),
            ),
        ];
        for (profile, traits, sync) in refused {
            assert!(
                check_folder_contents(&profile, traits, &sync).is_err(),
                "{profile} {sync}"
            );
        }
    }

    #[test]
    fn a_folder_read_is_stamped_with_its_encrypted_name() {
        let key = SymKey::generate();
        let (first, second) = (
            key.encrypt_str("Work").unwrap(),
            key.encrypt_str("Work").unwrap(),
        );
        let sync = json!({
            "folders": [{"id": "f1", "name": first}, {"id": "f2", "name": second}],
            "ciphers": [],
        });
        let vault = read_vault(&sync, &Scope::Personal, &key).unwrap();
        assert_eq!(vault.folders[0].stamp, Some(encrypted_name_hash(&first)));
        assert_ne!(vault.folders[0].stamp, vault.folders[1].stamp);
        assert_eq!(vault.folders[0].name, vault.folders[1].name);
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
        assert!(!enabled.organization_updates_safe);

        let vaultwarden = server_traits(&json!({
            "server": {"name": "Vaultwarden", "url": "https://github.com/dani-garcia/vaultwarden"},
            "featureStates": {}
        }));
        assert!(vaultwarden.hidden_item_types.is_empty());
        assert!(!vaultwarden.items_need_collections);
        assert!(vaultwarden.organization_updates_safe);
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
            "encryptedFor": enc("synthetic per-account marker"),
            "futureServerFlag": true,
            "futurePlainText": "per-account setting",
            "futurePlainNumber": 3,
            "futurePlainList": [true, "per-account setting", 3],
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
    fn discarded_encrypted_fields_stop_the_vault_read() {
        let key = SymKey::generate();
        let secret = "synthetic future secret";
        let encrypted = key.encrypt_str(secret).unwrap();
        for value in [
            json!(encrypted),
            json!([encrypted]),
            json!([null, {"nested": [false, {"value": encrypted}]}]),
        ] {
            let mut cipher = full_cipher(&key);
            cipher["futureUnknownField"] = value;
            let sync = json!({"ciphers": [cipher], "folders": [], "collections": []});
            let Err(error) = read_vault(&sync, &Scope::Personal, &key) else {
                panic!("a vault read silently discarded encrypted content");
            };
            let message = format!("{error:#}");
            assert!(
                message.contains("not supported"),
                "unsupported content was not identified"
            );
            assert!(
                !message.contains(secret),
                "the error exposed decrypted content"
            );
            assert!(
                !message.contains(&encrypted),
                "the error exposed ciphertext"
            );
            assert!(
                !message.contains("futureUnknownField"),
                "the error exposed an unknown field name"
            );
        }
    }

    #[tokio::test]
    async fn discarded_encrypted_fields_stop_backup_before_acceptance_or_pruning() {
        use crate::{
            api::tests::{http_response, http_server},
            backup::{BackupOptions, backup},
            mirror::Reporter,
            snapshot,
            store::{Keys, Store},
        };

        struct Quiet;
        impl Reporter for Quiet {
            fn status(&self, _: &str, _: &str) {}
            fn warn(&self, _: &str) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), "synthetic passphrase").unwrap();
        let store = Store::new(dir.path(), "source", &keys);
        let now = time::OffsetDateTime::now_utc();
        let document = snapshot::build(&Vault::default(), &Scope::Personal).unwrap();
        let old = store
            .begin(now - time::Duration::days(60))
            .unwrap()
            .finish(&document)
            .unwrap();
        let key = SymKey::generate();
        let cipher = json!({
            "id": "item-1", "type": 1,
            "name": key.encrypt_str("synthetic item").unwrap(),
            "futureUnknownField": key.encrypt_str("synthetic future secret").unwrap(),
        });
        let sync = json!({"ciphers": [cipher], "folders": [], "collections": []});
        let (url, requests) = http_server(vec![
            http_response("200 OK", "", r#"{"access_token":"synthetic-token"}"#),
            http_response("200 OK", "", "{}"),
            http_response("200 OK", "", &sync.to_string()),
            http_response("200 OK", "", "{}"),
        ]);
        let account = account(&url, key).await;
        let result = backup(
            &account,
            &Scope::Personal,
            &store,
            &BackupOptions {
                retention_days: 1,
                now,
            },
            &Quiet,
        )
        .await;
        assert!(
            result.is_err(),
            "a backup accepted a vault with discarded encrypted content"
        );
        assert_eq!(store.list().unwrap(), [old]);
        assert_eq!(requests.join().unwrap().len(), 3);
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
    fn malformed_optional_metadata_stops_the_vault_read() {
        let key = SymKey::generate();
        for name in [
            "folderId",
            "archivedDate",
            "deletedDate",
            "revisionDate",
            "creationDate",
            "key",
        ] {
            for value in [
                json!(7),
                json!(false),
                json!({"secret": "synthetic-metadata"}),
                json!(["synthetic-metadata"]),
            ] {
                let mut cipher = full_cipher(&key);
                cipher[name] = value;
                let sync = json!({"folders": [], "ciphers": [cipher]});
                let Err(error) = read_vault(&sync, &Scope::Personal, &key) else {
                    panic!("malformed {name} was treated as absent");
                };
                let message = format!("{error:#}");
                assert!(
                    message.contains(name),
                    "the invalid field was not identified"
                );
                assert!(
                    !message.contains("synthetic-metadata"),
                    "metadata was exposed"
                );
            }
        }
    }

    #[test]
    fn optional_metadata_preserves_supported_absent_forms() {
        let key = SymKey::generate();
        for value in [None, Some(Value::Null), Some(json!(""))] {
            let mut cipher = full_cipher(&key);
            for name in [
                "folderId",
                "archivedDate",
                "deletedDate",
                "revisionDate",
                "creationDate",
                "key",
            ] {
                match &value {
                    Some(value) => {
                        cipher[name] = value.clone();
                    }
                    None => {
                        cipher.as_object_mut().unwrap().remove(name);
                    }
                }
            }
            let item = decrypt_item(&cipher, &key).unwrap();
            assert!(item.folder_id.is_none());
            assert!(item.revision_date.is_none());
            assert!(item.creation_date.is_none());
            assert!(item.wrapped_key.is_none());
            assert!(!item.in_trash && !item.archived);
        }
    }

    #[test]
    fn malformed_attachment_and_collection_metadata_is_not_discarded() {
        let key = SymKey::generate();
        let mut cipher = full_cipher(&key);
        cipher["attachments"] = json!([{
            "id": "attachment-1",
            "fileName": key.encrypt_str("synthetic.txt").unwrap(),
            "size": 1,
            "key": {"secret": "synthetic-metadata"},
        }]);
        let personal = json!({"folders": [], "ciphers": [cipher]});
        let organization = json!({"collections": [{
            "id": "collection-1",
            "organizationId": "org-1",
            "name": key.encrypt_str("synthetic collection").unwrap(),
            "externalId": ["synthetic-metadata"],
        }], "ciphers": []});
        for (sync, scope, name) in [
            (personal, Scope::Personal, "key"),
            (
                organization,
                Scope::Organization("org-1".into()),
                "externalId",
            ),
        ] {
            let Err(error) = read_vault(&sync, &scope, &key) else {
                panic!("malformed {name} was discarded");
            };
            let message = format!("{error:#}");
            assert!(
                message.contains(name),
                "the invalid field was not identified"
            );
            assert!(
                !message.contains("synthetic-metadata"),
                "metadata was exposed"
            );
        }
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
    fn a_known_encrypted_slot_holds_ciphertext_or_is_an_error() {
        let key = SymKey::generate();
        for pointer in [
            "/name",
            "/notes",
            "/login/password",
            "/login/totp",
            "/login/uris/0/uri",
            "/login/fido2Credentials/0/keyValue",
            "/fields/0/name",
            "/fields/1/value",
            "/passwordHistory/0/password",
            "/card/number",
            "/identity/ssn",
            "/sshKey/privateKey",
            "/bankAccount/swiftCode",
            "/driversLicense/licenseNumber",
            "/passport/expirationDate",
        ] {
            for value in [json!("2.AAAA"), json!("hunter2"), json!(42)] {
                let mut cipher = full_cipher(&key);
                *cipher.pointer_mut(pointer).unwrap() = value.clone();
                let Err(error) = decrypt_item(&cipher, &key) else {
                    panic!("{pointer} = {value} was read as plain content");
                };
                let message = format!("{error:#}");
                let (field, rest) = pointer[1..].split_once('/').unwrap_or((&pointer[1..], ""));
                assert!(message.contains(&format!("field {field}")), "{message}");
                if !rest.is_empty() {
                    assert!(message.contains(&format!(" at /{rest}")), "{message}");
                }
                assert!(message.contains("unsupported format"), "{message}");
                assert!(!message.contains("AAAA") && !message.contains("hunter2"));
            }
        }
    }

    #[test]
    fn an_item_stored_as_an_encrypted_data_blob_is_refused() {
        let key = SymKey::generate();
        let blob = key.encrypt_str(r#"{"name":"GitHub"}"#).unwrap();
        for cipher in [
            json!({"id": "item-1", "type": 1, "data": blob}),
            json!({"id": "item-1", "type": 1, "name": key.encrypt_str("n").unwrap(), "data": blob}),
        ] {
            let Err(error) = decrypt_item(&cipher, &key) else {
                panic!("an encrypted data blob was dropped");
            };
            let message = format!("{error:#}");
            assert!(message.contains("data"), "{message}");
            assert!(message.contains("not supported"), "{message}");
            assert!(!message.contains(&blob[2..12]), "{message}");
        }
    }

    #[test]
    fn the_legacy_data_duplicate_is_read_as_either_server_sends_it() {
        let key = SymKey::generate();
        let name = key.encrypt_str("GitHub").unwrap();
        // Bitwarden's server sends the duplicate as a JSON string.
        let legacy = json!({"Fields": null, "Name": name, "Type": 1});
        for data in [Value::Null, legacy.clone(), json!(legacy.to_string())] {
            let mut cipher = full_cipher(&key);
            cipher["data"] = data;
            assert_eq!(decrypt_item(&cipher, &key).unwrap().name(), "2.AAAA");
        }
    }

    #[test]
    fn an_item_type_block_that_is_not_an_object_is_an_error() {
        let key = SymKey::generate();
        let enc = key.encrypt_str("x").unwrap();
        for block in [
            "login",
            "secureNote",
            "card",
            "identity",
            "sshKey",
            "bankAccount",
            "driversLicense",
            "passport",
        ] {
            for value in [
                json!("plain"),
                json!(enc),
                json!([enc]),
                json!(5),
                json!(true),
            ] {
                let mut cipher = full_cipher(&key);
                cipher[block] = value.clone();
                let Err(error) = decrypt_item(&cipher, &key) else {
                    panic!("{block} = {value} was left out of the item");
                };
                let message = format!("{error:#}");
                assert!(message.contains(&format!("field {block}")), "{message}");
                assert!(!message.contains("plain") && !message.contains(&enc[2..12]));
            }
            for value in [Value::Null, json!(""), json!([])] {
                let mut cipher = full_cipher(&key);
                cipher[block] = value;
                decrypt_item(&cipher, &key).unwrap_or_else(|error| panic!("{block}: {error:#}"));
            }
        }
    }

    #[test]
    fn item_lists_of_the_wrong_shape_are_errors_not_empty() {
        let key = SymKey::generate();
        let field = json!({"name": "2.AAAA", "value": "sentinel"});
        for (pointer, value) in [
            ("/fields", field.clone()),
            ("/fields", json!("sentinel")),
            ("/fields", json!(["sentinel"])),
            ("/fields", json!([field, 5])),
            ("/passwordHistory", json!({"password": "sentinel"})),
            ("/passwordHistory", json!([["sentinel"]])),
            ("/login/uris", json!({"uri": "sentinel"})),
            ("/login/uris", json!(["sentinel"])),
            ("/login/fido2Credentials", json!("sentinel")),
            ("/login/fido2Credentials", json!([true])),
        ] {
            let mut cipher = full_cipher(&key);
            *cipher.pointer_mut(pointer).unwrap() = value.clone();
            let Err(error) = decrypt_item(&cipher, &key) else {
                panic!("{pointer} = {value} was read as an item list");
            };
            let message = format!("{error:#}");
            assert!(message.contains(&pointer[1..]), "{message}");
            assert!(!message.contains("sentinel") && !message.contains("AAAA"));
        }
        for pointer in [
            "/fields",
            "/passwordHistory",
            "/login/uris",
            "/login/fido2Credentials",
        ] {
            for value in [Value::Null, json!([])] {
                let mut cipher = full_cipher(&key);
                *cipher.pointer_mut(pointer).unwrap() = value;
                decrypt_item(&cipher, &key).unwrap_or_else(|error| panic!("{pointer}: {error:#}"));
            }
        }
    }

    #[test]
    fn malformed_attachment_and_collection_lists_are_errors_not_empty() {
        let key = SymKey::generate();
        for (name, value) in [
            ("attachments", json!({})),
            ("attachments", json!("att-1")),
            ("collectionIds", json!("c1")),
            ("collectionIds", json!([1])),
            ("collectionIds", json!(["c1", null])),
        ] {
            let mut cipher = full_cipher(&key);
            cipher[name] = value.clone();
            let Err(error) = decrypt_item(&cipher, &key) else {
                panic!("{name} = {value} was read as a list");
            };
            assert!(format!("{error:#}").contains(name), "{error:#}");
        }
        for (name, value) in [
            ("attachments", Value::Null),
            ("attachments", json!([])),
            ("collectionIds", Value::Null),
            ("collectionIds", json!(["c1"])),
        ] {
            let mut cipher = full_cipher(&key);
            cipher[name] = value;
            decrypt_item(&cipher, &key).unwrap();
        }
    }

    #[test]
    fn malformed_ownership_is_an_error_not_another_scope() {
        let key = SymKey::generate();
        let org = Scope::Organization("org-1".into());
        let sync = |cipher_owner: Value, collection_owner: Value| {
            let mut cipher = full_cipher(&key);
            cipher["organizationId"] = cipher_owner;
            json!({
                "folders": [],
                "collections": [{
                    "id": "c1",
                    "organizationId": collection_owner,
                    "name": key.encrypt_str("Shared").unwrap(),
                }],
                "ciphers": [cipher],
            })
        };
        let personal = read_vault(&sync(Value::Null, json!("org-1")), &Scope::Personal, &key);
        assert_eq!(personal.unwrap().items.len(), 1);
        let shared = read_vault(&sync(json!("org-1"), json!("org-1")), &org, &key).unwrap();
        assert_eq!((shared.items.len(), shared.collections.len()), (1, 1));
        let other = read_vault(&sync(json!("org-2"), json!("org-2")), &org, &key).unwrap();
        assert_eq!((other.items.len(), other.collections.len()), (0, 0));

        for malformed in [
            json!(7),
            json!({"id": "org-1"}),
            json!(["org-1"]),
            json!(""),
        ] {
            for scope in [&Scope::Personal, &org] {
                let item = read_vault(&sync(malformed.clone(), json!("org-1")), scope, &key);
                assert!(item.is_err(), "item owner {malformed} in {scope:?}");
            }
            let collection = read_vault(&sync(json!("org-1"), malformed.clone()), &org, &key);
            assert!(collection.is_err(), "collection owner {malformed}");
        }
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
