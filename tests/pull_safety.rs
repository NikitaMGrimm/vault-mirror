//! `run --pull`, and the deletions a run ends with, against a fake server in
//! this process. Unlike the sandbox servers, it can refuse an upload, drop part
//! of a write, or change an item between two requests on cue, and it records
//! every request that could change a vault, so a test can tell that a refused
//! run changed nothing.

mod common;

use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
};

use anyhow::Result;
use common::{Capture, by_name};
use serde_json::{Value, json};
use vault_mirror::{
    api::{ApiCredentials, ClientOptions, ServerUrls, build_http_client},
    crypto::{KdfParams, MasterSecret, SymKey, share_key_with},
    mirror::{self, Endpoint, Filter, MirrorError, MirrorOptions, Outcome, Pulled},
    node::Node,
    plan::{Mode, Safety, mark_copy, marker_of},
    vault::{Account, Draft, Item, Scope, Vault},
};

const EMAIL: &str = "fake@example.com";
const PASSWORD: &str = "fake server master password";
const PERSONAL: Scope = Scope::Personal;

/// One account on the fake server, with its individual vault and one
/// organization.
struct Side {
    profile: Value,
    folders: Vec<Value>,
    collections: Vec<Value>,
    ciphers: Vec<Value>,
    files: HashMap<String, Vec<u8>>,
    premium: bool,
    /// Answer as Bitwarden's server, and leave bank accounts, driver's
    /// licences and passports out of syncs as it does unless a feature flag is
    /// on. Otherwise answer as Vaultwarden.
    hiding_new_types: bool,
    /// Answer every attachment upload with a server error.
    failing_uploads: bool,
    /// Store every uploaded file, then answer with a server error, as when
    /// the answer gets lost.
    unanswered_uploads: bool,
    /// Give a new file of an item that has files the id of its first one.
    reusing_attachment_ids: bool,
    /// Store every uploaded file with its last byte changed, and say nothing.
    corrupting_uploads: bool,
    /// Store items without their notes, and say nothing.
    dropping_notes: bool,
    cleanup_fault: Option<&'static str>,
    altered_container_name: Option<Value>,
    drop_collection_external_id: bool,
    dropping_collections: bool,
    trashing_writes: bool,
    dropping_files_after_upload: bool,
    drop_files_on_next_update: bool,
    /// Every request that can change the vault, as "METHOD /path".
    writes: Vec<String>,
}

#[derive(Default)]
struct State {
    sides: BTreeMap<String, Side>,
    /// Makes ids and revision dates unique.
    counter: u64,
}

struct Fake {
    port: u16,
    state: Arc<Mutex<State>>,
}

impl Fake {
    fn start() -> Self {
        let master =
            MasterSecret::derive(PASSWORD, EMAIL, &KdfParams::Pbkdf2 { iterations: 5000 }).unwrap();
        let keys = master.new_account_keys().unwrap();
        let mut state = State::default();
        for name in ["source", "dest"] {
            let org_key = share_key_with(&keys.public_key, &SymKey::generate()).unwrap();
            let profile = json!({
                "id": format!("user-{name}"),
                "email": EMAIL,
                "key": keys.wrapped_user_key,
                "privateKey": keys.wrapped_private_key,
                "organizations": [{
                    "id": format!("org-{name}"),
                    "key": org_key,
                    "organizationUserId": format!("member-{name}"),
                }],
            });
            let side = Side {
                profile,
                folders: Vec::new(),
                collections: Vec::new(),
                ciphers: Vec::new(),
                files: HashMap::new(),
                premium: true,
                hiding_new_types: false,
                failing_uploads: false,
                unanswered_uploads: false,
                reusing_attachment_ids: false,
                corrupting_uploads: false,
                dropping_notes: false,
                cleanup_fault: None,
                altered_container_name: None,
                drop_collection_external_id: false,
                dropping_collections: false,
                trashing_writes: false,
                dropping_files_after_upload: false,
                drop_files_on_next_update: false,
                writes: Vec::new(),
            };
            state.sides.insert(name.to_string(), side);
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(state));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve(stream, &state, port));
            }
        });
        Self { port, state }
    }

    async fn open(&self, side: &str) -> Account {
        let urls = ServerUrls::from_server(&format!("http://127.0.0.1:{}/{side}", self.port));
        let credentials = ApiCredentials {
            client_id: format!("user.{side}"),
            client_secret: "fake secret".into(),
            device_id: "fake device".into(),
        };
        let http = build_http_client(&ClientOptions::default()).unwrap();
        Account::open(http, urls.unwrap(), credentials, PASSWORD)
            .await
            .unwrap()
    }

    fn side<T>(&self, side: &str, change: impl FnOnce(&mut Side) -> T) -> T {
        change(self.state.lock().unwrap().sides.get_mut(side).unwrap())
    }

    fn writes(&self, side: &str) -> Vec<String> {
        self.side(side, |side| side.writes.clone())
    }

    fn deletes(&self, side: &str) -> Vec<String> {
        let mut writes = self.writes(side);
        writes.retain(|write| write.starts_with("DELETE"));
        writes
    }

    fn forget_writes(&self) {
        for side in self.state.lock().unwrap().sides.values_mut() {
            side.writes.clear();
        }
    }

    /// Changes the stored item `id` behind the client's back.
    fn change_item(&self, side: &str, id: &str, change: impl FnOnce(&mut Value)) {
        self.side(side, |side| {
            change(side.ciphers.iter_mut().find(|c| c["id"] == id).unwrap())
        });
    }
}

struct Request {
    method: String,
    path: String,
    content_type: String,
    body: Vec<u8>,
}

fn serve(mut stream: TcpStream, state: &Mutex<State>, port: u16) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let (status, content_type, body) = match respond(state, port, &request) {
        Ok(Answer::Json(value)) => (200, "application/json", value.to_string().into_bytes()),
        Ok(Answer::Bytes(bytes)) => (200, "application/octet-stream", bytes),
        Err((status, message)) => (
            status,
            "application/json",
            json!({ "message": message }).to_string().into_bytes(),
        ),
    };
    let head = format!(
        "HTTP/1.1 {status} Fake\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = [0; 16384];
    let mut more = |data: &mut Vec<u8>| -> Option<()> {
        let read = stream.read(&mut buffer).ok()?;
        (read > 0).then(|| data.extend_from_slice(&buffer[..read]))
    };
    let mut data = Vec::new();
    let end = loop {
        if let Some(end) = find(&data, b"\r\n\r\n") {
            break end;
        }
        more(&mut data)?;
    };
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.split('?').next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let mut body = data.split_off(end + 4);
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        body = loop {
            if let Some(body) = dechunk(&body) {
                break body;
            }
            more(&mut body)?;
        };
    } else {
        let length = (headers.get("content-length"))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        while body.len() < length {
            more(&mut body)?;
        }
    }
    Some(Request {
        method,
        path,
        content_type: headers.get("content-type").cloned().unwrap_or_default(),
        body,
    })
}

/// The body of a chunked request, once all of it has arrived.
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = find(data, b"\r\n")?;
        let size = std::str::from_utf8(&data[..line]).ok()?.split(';').next()?;
        let size = usize::from_str_radix(size.trim(), 16).ok()?;
        data = &data[line + 2..];
        if size == 0 {
            return Some(body);
        }
        if data.len() < size + 2 {
            return None;
        }
        body.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

fn uploaded_file(request: &Request) -> Option<Vec<u8>> {
    let boundary = request.content_type.split("boundary=").nth(1)?;
    let boundary = format!("\r\n--{}", boundary.trim_matches('"'));
    let start = find(&request.body, b"\r\n\r\n")? + 4;
    let end = start + find(&request.body[start..], boundary.as_bytes())?;
    Some(request.body[start..end].to_vec())
}

enum Answer {
    Json(Value),
    Bytes(Vec<u8>),
}

type Refusal = (u16, String);

fn cipher<'a>(side: &'a mut Side, id: &str) -> Result<&'a mut Value, Refusal> {
    (side.ciphers.iter_mut())
        .find(|cipher| cipher["id"] == id)
        .ok_or((404, format!("item {id} not found")))
}

fn ids(body: &Value) -> Vec<String> {
    (body["ids"].as_array().into_iter().flatten())
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect()
}

fn store(side: &mut Side, mut cipher: Value) -> Value {
    let object = cipher.as_object_mut().unwrap();
    object.remove("lastKnownRevisionDate");
    object.remove("encryptedFor");
    if side.dropping_notes {
        object.remove("notes");
    }
    cipher
}

fn respond(state: &Mutex<State>, port: u16, request: &Request) -> Result<Answer, Refusal> {
    let mut state = state.lock().unwrap();
    state.counter += 1;
    let n = state.counter;
    let stamp = format!("2026-10-16T12:00:00.{n:07}Z");
    let (name, rest) = request.path[1..].split_once('/').unwrap_or_default();
    let name = name.to_string();
    let segments: Vec<&str> = rest.split('/').collect();
    let method = request.method.as_str();
    let side = (state.sides.get_mut(&name)).ok_or((404, format!("no account {name}")))?;
    if method != "GET" && segments != ["identity", "connect", "token"] {
        side.writes.push(format!("{method} /{rest}"));
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let id = format!("{name}-{n}");
    let value = match (method, segments.as_slice()) {
        ("POST", ["identity", "connect", "token"]) => {
            json!({ "access_token": format!("token-{name}"), "Kdf": 0, "KdfIterations": 5000 })
        }
        ("GET", ["api", "accounts", "profile"]) => {
            let mut profile = side.profile.clone();
            profile["premium"] = json!(side.premium);
            profile
        }
        ("GET", ["api", "config"]) if side.hiding_new_types => {
            json!({ "server": null, "featureStates": {} })
        }
        ("GET", ["api", "config"]) => json!({ "server": { "name": "Vaultwarden" } }),
        ("GET", ["api", "sync"]) => {
            let hidden = |cipher: &&Value| {
                side.hiding_new_types && [6, 7, 8].contains(&cipher["type"].as_u64().unwrap_or(0))
            };
            let shown: Vec<&Value> = side.ciphers.iter().filter(|c| !hidden(c)).collect();
            json!({ "folders": side.folders, "collections": side.collections, "ciphers": shown })
        }
        ("POST", ["api", "folders"]) => {
            let folder = json!({ "id": id, "name": side.altered_container_name.as_ref().unwrap_or(&body["name"]), "revisionDate": stamp });
            side.folders.push(folder.clone());
            folder
        }
        ("DELETE", ["api", "folders", folder]) => {
            side.folders.retain(|f| f["id"] != *folder);
            Value::Null
        }
        ("GET", ["api", "organizations", _, "collections"]) => json!({ "data": side.collections }),
        ("POST", ["api", "organizations", org, "collections"]) => {
            let collection = json!({
                "id": id,
                "organizationId": org,
                "name": side.altered_container_name.as_ref().unwrap_or(&body["name"]),
                "externalId": if side.drop_collection_external_id { Value::Null } else { body["externalId"].clone() },
            });
            side.collections.push(collection.clone());
            collection
        }
        ("DELETE", ["api", "organizations", _, "collections", collection]) => {
            side.collections.retain(|c| c["id"] != *collection);
            Value::Null
        }
        ("GET", ["api", "ciphers", "organization-details"]) => {
            let owned = (side.ciphers.iter()).filter(|c| c["organizationId"].is_string());
            json!({ "data": owned.collect::<Vec<_>>() })
        }
        ("POST", ["api", "ciphers"]) | ("POST", ["api", "ciphers", "create"]) => {
            let (cipher, collections) = match segments.len() {
                2 => (body, json!([])),
                _ => (body["cipher"].clone(), body["collectionIds"].clone()),
            };
            let mut cipher = store(side, cipher);
            let object = cipher.as_object_mut().unwrap();
            object.insert("id".into(), json!(id));
            object.insert("revisionDate".into(), json!(stamp));
            object.insert("creationDate".into(), json!(stamp));
            object.insert(
                "deletedDate".into(),
                if side.trashing_writes {
                    json!(stamp)
                } else {
                    Value::Null
                },
            );
            object.insert("attachments".into(), json!([]));
            object.insert(
                "collectionIds".into(),
                if side.dropping_collections {
                    json!([])
                } else {
                    collections
                },
            );
            object.entry("archivedDate").or_insert(Value::Null);
            side.ciphers.push(cipher.clone());
            cipher
        }
        ("PUT", ["api", "ciphers", "delete"]) => {
            for id in ids(&body) {
                let cipher = cipher(side, &id)?;
                cipher["deletedDate"] = json!(stamp);
                cipher["revisionDate"] = json!(stamp);
            }
            Value::Null
        }
        ("PUT", ["api", "ciphers", action @ ("archive" | "unarchive")]) => {
            for id in ids(&body) {
                let cipher = cipher(side, &id)?;
                cipher["archivedDate"] = match *action {
                    "archive" => json!(stamp),
                    _ => Value::Null,
                };
                cipher["revisionDate"] = json!(stamp);
            }
            Value::Null
        }
        ("DELETE", ["api", "ciphers"]) => {
            let ids = ids(&body);
            side.ciphers
                .retain(|c| !ids.iter().any(|id| c["id"] == id.as_str()));
            Value::Null
        }
        ("GET", ["api", "ciphers", item]) => cipher(side, item)?.clone(),
        ("PUT", ["api", "ciphers", item]) => {
            let updated = store(side, body.clone());
            let drop_files = std::mem::take(&mut side.drop_files_on_next_update);
            let trashing = side.trashing_writes;
            let cleanup_fault = side.cleanup_fault;
            let existing = cipher(side, item)?;
            if body["lastKnownRevisionDate"] != existing["revisionDate"] {
                return Err((400, "The client copy of this cipher is out of date.".into()));
            }
            let mut updated = updated;
            let cleaning = body["fields"].as_array().map_or(0, Vec::len)
                < existing["fields"].as_array().map_or(0, Vec::len);
            for key in [
                "id",
                "creationDate",
                "deletedDate",
                "attachments",
                "collectionIds",
            ] {
                updated[key] = existing[key].clone();
            }
            updated["revisionDate"] = json!(stamp);
            if drop_files {
                updated["attachments"] = json!([]);
            }
            if trashing {
                updated["deletedDate"] = json!(stamp);
            }
            if cleaning {
                match cleanup_fault {
                    Some("notes") => {
                        updated.as_object_mut().unwrap().remove("notes");
                    }
                    Some("folder") => updated["folderId"] = Value::Null,
                    Some("files") => updated["attachments"] = json!([]),
                    Some("trash") => updated["deletedDate"] = json!(stamp),
                    Some("archive") => updated["archivedDate"] = json!(stamp),
                    Some("collections") => updated["collectionIds"] = json!([]),
                    _ => {}
                }
            }
            *existing = updated.clone();
            if cleaning && cleanup_fault == Some("file-bytes") {
                for file in side.files.values_mut() {
                    *file.last_mut().unwrap() ^= 1;
                }
            }
            if cleaning && cleanup_fault == Some("ambiguous") {
                return Err((500, "the answer got lost".into()));
            }
            updated
        }
        ("PUT", ["api", "ciphers", item, "collections"]) => {
            let dropping = side.dropping_collections;
            let existing = cipher(side, item)?;
            existing["collectionIds"] = if dropping {
                json!([])
            } else {
                body["collectionIds"].clone()
            };
            existing["revisionDate"] = json!(stamp);
            Value::Null
        }
        ("POST", ["api", "ciphers", item, "attachment", "v2"]) => {
            let (premium, reusing) = (side.premium, side.reusing_attachment_ids);
            let existing = cipher(side, item)?;
            if !premium && !existing["organizationId"].is_string() {
                return Err((
                    400,
                    "You must have premium status to use attachments.".into(),
                ));
            }
            if let Some(first) = existing["attachments"].get(0).filter(|_| reusing) {
                return Ok(Answer::Json(
                    json!({ "attachmentId": first["id"], "fileUploadType": 0, "url": "" }),
                ));
            }
            existing["attachments"].as_array_mut().unwrap().push(json!({
                "id": id,
                "fileName": body["fileName"],
                "key": body["key"],
                "size": body["fileSize"].to_string(),
                "sizeName": "",
                "url": null,
            }));
            existing["revisionDate"] = json!(stamp);
            json!({ "attachmentId": id, "fileUploadType": 0, "url": "", "cipherResponse": existing })
        }
        ("POST", ["api", "ciphers", item, "attachment", file]) => {
            if side.failing_uploads {
                return Err((500, "the upload failed".into()));
            }
            let mut data = uploaded_file(request).ok_or((400, "no file".to_string()))?;
            if side.corrupting_uploads {
                *data.last_mut().unwrap() ^= 1;
            }
            let listed = cipher(side, item)?["attachments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == *file);
            if !listed {
                return Err((404, format!("attachment {file} not found")));
            }
            side.files.insert(file.to_string(), data);
            side.drop_files_on_next_update = side.dropping_files_after_upload;
            if side.unanswered_uploads {
                return Err((500, "the answer got lost".into()));
            }
            Value::Null
        }
        ("GET", ["api", "ciphers", item, "attachment", file]) => {
            let attachments = cipher(side, item)?["attachments"].as_array().unwrap();
            let mut attachment = (attachments.iter())
                .find(|a| a["id"] == *file)
                .cloned()
                .ok_or((404, format!("attachment {file} not found")))?;
            attachment["url"] = json!(format!("http://127.0.0.1:{port}/{name}/files/{file}"));
            attachment
        }
        ("DELETE", ["api", "ciphers", item, "attachment", file]) => {
            let existing = cipher(side, item)?;
            (existing["attachments"].as_array_mut().unwrap()).retain(|a| a["id"] != *file);
            existing["revisionDate"] = json!(stamp);
            side.files.remove(*file);
            Value::Null
        }
        ("GET", ["files", file]) => {
            let data = side.files.get(*file).cloned();
            return data
                .map(Answer::Bytes)
                .ok_or((404, format!("file {file} not found")));
        }
        _ => {
            return Err((
                404,
                format!("{method} /{rest} is not something the fake answers"),
            ));
        }
    };
    Ok(Answer::Json(value))
}

struct Vaults {
    fake: Fake,
    source: Account,
    dest: Account,
}

fn personal(account: &Account) -> Endpoint<'_> {
    Endpoint {
        account,
        scope: PERSONAL,
    }
}

fn note(name: &str) -> BTreeMap<String, Node> {
    let kind = BTreeMap::from([("type".to_string(), Node::Number(0.into()))]);
    BTreeMap::from([
        ("type".to_string(), Node::Number(2.into())),
        ("name".to_string(), Node::secret(name)),
        (
            "notes".to_string(),
            Node::secret(format!("{name} as written")),
        ),
        ("secureNote".to_string(), Node::Map(kind)),
    ])
}

fn passport(name: &str) -> BTreeMap<String, Node> {
    let passport = [("passportNumber", "C01X00T47"), ("surname", "Mustermann")]
        .map(|(key, value)| (key.to_string(), Node::secret(value)));
    BTreeMap::from([
        ("type".to_string(), Node::Number(8.into())),
        ("name".to_string(), Node::secret(name)),
        ("passport".to_string(), Node::Map(passport.into())),
    ])
}

fn folder_id(vault: &Vault, name: &str) -> String {
    let folder = vault.folders.iter().find(|f| f.name == name).unwrap();
    folder.id.clone()
}

fn notes(item: &Item) -> Option<&str> {
    item.content.get("notes").and_then(Node::as_str)
}

fn folders(include: &[&str], exclude: &[&str]) -> Filter {
    let names = |list: &[&str]| list.iter().map(|name| name.to_string()).collect();
    Filter {
        include: names(include),
        exclude: names(exclude),
    }
}

/// A source with two notes in the folder "Work", one in "Private" and one in
/// none, the first of them with a file, mirrored into the destination.
async fn mirrored() -> Vaults {
    let fake = Fake::start();
    let source = fake.open("source").await;
    let dest = fake.open("dest").await;
    let work = source.create_folder("Work").await.unwrap().id;
    let private = source.create_folder("Private").await.unwrap().id;
    for (name, folder) in [
        ("Work note", Some(&work)),
        ("Work spare", Some(&work)),
        ("Private note", Some(&private)),
        ("Loose note", None),
    ] {
        let draft = Draft {
            content: note(name),
            folder_id: folder.cloned(),
            collection_ids: Vec::new(),
        };
        let item = source.create_item(&PERSONAL, &draft).await.unwrap();
        if name == "Work note" {
            source
                .upload_attachment(&PERSONAL, &item, "file.txt", b"first")
                .await
                .unwrap();
        }
    }
    mirror::mirror(
        &personal(&source),
        &personal(&dest),
        &MirrorOptions::default(),
        &Capture::default(),
    )
    .await
    .unwrap();
    Vaults { fake, source, dest }
}

/// Changes the item named `name` as a user would in a client.
async fn edit(account: &Account, name: &str, change: impl FnOnce(&mut Draft)) -> Item {
    let vault = account.load(&PERSONAL).await.unwrap();
    let item = by_name(&vault, name);
    let mut draft = Draft {
        content: item.content.clone(),
        folder_id: item.folder_id.clone(),
        collection_ids: item.collection_ids.clone(),
    };
    change(&mut draft);
    account.update_item(&PERSONAL, item, &draft).await.unwrap()
}

fn set_notes(text: &str) -> impl FnOnce(&mut Draft) {
    move |draft| {
        draft
            .content
            .insert("notes".into(), Node::secret(text.to_string()));
    }
}

/// Replaces the file of the item named `name` with one of the same name.
async fn replace_file(account: &Account, name: &str, data: &[u8]) {
    let vault = account.load(&PERSONAL).await.unwrap();
    let item = by_name(&vault, name);
    for attachment in &item.attachments {
        account
            .delete_attachment(&item.id, &attachment.id)
            .await
            .unwrap();
    }
    let item = account.get_item(&PERSONAL, &item.id).await.unwrap();
    account
        .upload_attachment(&PERSONAL, &item, "file.txt", data)
        .await
        .unwrap();
}

async fn files(account: &Account, name: &str) -> Vec<Vec<u8>> {
    let vault = account.load(&PERSONAL).await.unwrap();
    let item = by_name(&vault, name);
    let mut files = Vec::new();
    for attachment in &item.attachments {
        let data = account.download_attachment(&PERSONAL, item, attachment);
        files.push(data.await.unwrap());
    }
    files.sort();
    files
}

async fn file_ids(account: &Account, name: &str) -> Vec<String> {
    let vault = account.load(&PERSONAL).await.unwrap();
    let item = by_name(&vault, name);
    item.attachments.iter().map(|a| a.id.clone()).collect()
}

async fn file(account: &Account, name: &str, id: &str) -> Vec<u8> {
    let vault = account.load(&PERSONAL).await.unwrap();
    let item = by_name(&vault, name);
    let attachment = item.attachments.iter().find(|a| a.id == id).unwrap();
    let data = account.download_attachment(&PERSONAL, item, attachment);
    data.await.unwrap()
}

/// Each way an upload fails, to be switched on and off: the server refuses
/// the file, or takes it and its answer gets lost.
fn upload_failures() -> [fn(&mut Side, bool); 2] {
    [
        |side, failing| side.failing_uploads = failing,
        |side, failing| side.unanswered_uploads = failing,
    ]
}

fn pull_options(filter: Filter) -> MirrorOptions {
    MirrorOptions {
        filter,
        // As `run --pull` sets it.
        safety: Safety {
            allow_reverse: true,
            ..Safety::default()
        },
        ..MirrorOptions::default()
    }
}

/// Pulls from the vaults as they are now, after forgetting earlier writes.
async fn pull(vaults: &Vaults, filter: Filter) -> Result<Pulled> {
    let source_vault = vaults.source.load(&PERSONAL).await?;
    let dest_vault = vaults.dest.load(&PERSONAL).await?;
    vaults.fake.forget_writes();
    mirror::pull(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        &source_vault,
        &dest_vault,
        &pull_options(filter),
        true,
        &Capture::default(),
    )
    .await
}

/// A run without `--pull`, after forgetting earlier writes.
async fn run(vaults: &Vaults, options: &MirrorOptions) -> Result<Outcome, MirrorError> {
    vaults.fake.forget_writes();
    let (source, dest) = (personal(&vaults.source), personal(&vaults.dest));
    mirror::mirror(&source, &dest, options, &Capture::default()).await
}

fn no_writes() -> Vec<String> {
    Vec::new()
}

/// An edit to a copy whose original the filter leaves out stops the pull. It
/// must not be skipped either: the mirror after the pull would delete that
/// copy, and the edit with it.
#[tokio::test]
async fn a_pull_stops_at_an_edited_copy_its_filter_leaves_out() {
    let vaults = mirrored().await;
    let copy = edit(&vaults.dest, "Private note", set_notes("edited")).await;

    let error = pull(&vaults, folders(&["Work"], &[])).await.unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(&copy.id), "{message}");
    assert!(message.contains("INCLUDE_FOLDERS"), "{message}");
    assert_eq!(vaults.fake.writes("source"), no_writes());
}

/// Moving a copy into a folder the filter leaves out does not carry its
/// original out of the filter's reach.
#[tokio::test]
async fn a_pull_stops_at_a_copy_moved_out_of_its_filter() {
    let vaults = mirrored().await;
    let private = folder_id(&vaults.dest.load(&PERSONAL).await.unwrap(), "Private");
    let copy = edit(&vaults.dest, "Work note", |draft| {
        draft.folder_id = Some(private)
    })
    .await;

    let error = pull(&vaults, folders(&["Work"], &[])).await.unwrap_err();
    assert!(format!("{error:#}").contains(&copy.id), "{error:#}");
    assert_eq!(vaults.fake.writes("source"), no_writes());
    let source = vaults.source.load(&PERSONAL).await.unwrap();
    let original = by_name(&source, "Work note");
    assert_eq!(original.folder_id, Some(folder_id(&source, "Work")));
}

#[tokio::test]
async fn a_filtered_pull_brings_back_the_edits_it_selects() {
    let vaults = mirrored().await;
    edit(&vaults.dest, "Work spare", set_notes("edited")).await;

    let pulled = pull(&vaults, folders(&["Work"], &[])).await.unwrap();
    assert_eq!(pulled.summary.items_updated, 1);
    let source = vaults.source.load(&PERSONAL).await.unwrap();
    assert_eq!(notes(by_name(&source, "Work spare")), Some("edited"));
}

/// A copy edited after its original was deleted or trashed stops the pull: the
/// mirror after it would delete the copy and the edit. An unedited one doesn't.
#[tokio::test]
async fn a_pull_stops_at_an_edited_copy_whose_original_is_gone() {
    for trashed in [false, true] {
        let vaults = mirrored().await;
        let source = vaults.source.load(&PERSONAL).await.unwrap();
        let original = vec![by_name(&source, "Private note").id.clone()];
        match trashed {
            true => vaults.source.trash_items(&original).await.unwrap(),
            false => vaults.source.delete_items(&original).await.unwrap(),
        }
        pull(&vaults, Filter::default()).await.unwrap();

        let copy = edit(&vaults.dest, "Private note", set_notes("edited")).await;
        let error = pull(&vaults, Filter::default()).await.unwrap_err();
        assert!(format!("{error:#}").contains(&copy.id), "{error:#}");
        assert_eq!(vaults.fake.writes("source"), no_writes());
    }
}

/// A copy changed into an item type the source server would hide stops the
/// pull. Written into the source, the original would vanish from its syncs,
/// and the mirror after the pull would delete the copy.
#[tokio::test]
async fn a_pull_stops_at_a_copy_changed_into_a_type_the_source_hides() {
    let vaults = mirrored().await;
    vaults
        .fake
        .side("source", |side| side.hiding_new_types = true);
    let copy = edit(&vaults.dest, "Work spare", |draft| {
        let fields = draft.content.remove("fields").unwrap();
        draft.content = passport("Work spare");
        draft.content.insert("fields".into(), fields);
    })
    .await;

    let error = pull(&vaults, Filter::default()).await.unwrap_err();
    assert!(format!("{error:#}").contains(&copy.id), "{error:#}");
    assert_eq!(vaults.fake.writes("source"), no_writes());
}

/// Without premium the source cannot take a pulled file. The pull stops before
/// it changes the source, and the original keeps its file.
#[tokio::test]
async fn a_pull_into_a_source_without_premium_stops_before_changing_it() {
    for added in [false, true] {
        let mut vaults = mirrored().await;
        match added {
            true => {
                let draft = Draft {
                    content: note("Added in the destination"),
                    folder_id: None,
                    collection_ids: Vec::new(),
                };
                let item = vaults.dest.create_item(&PERSONAL, &draft).await.unwrap();
                (vaults.dest)
                    .upload_attachment(&PERSONAL, &item, "new.txt", b"new")
                    .await
                    .unwrap();
            }
            false => replace_file(&vaults.dest, "Work note", b"second").await,
        }
        vaults.fake.side("source", |side| side.premium = false);
        vaults.source = vaults.fake.open("source").await;

        let error = pull(&vaults, Filter::default()).await.unwrap_err();
        assert!(format!("{error:#}").contains("premium"), "{error:#}");
        assert_eq!(vaults.fake.writes("source"), no_writes());
        assert_eq!(files(&vaults.source, "Work note").await, [b"first"]);
    }
}

/// A pull that fails to upload a file deletes nothing: neither the original's
/// file nor the new one, which may have arrived. A run without `--pull` stops
/// rather than finish the item from the source. The next pull finishes it.
#[tokio::test]
async fn a_failed_upload_in_a_pull_keeps_the_file_it_was_replacing() {
    for fail in upload_failures() {
        let vaults = mirrored().await;
        let old = file_ids(&vaults.source, "Work note").await;
        replace_file(&vaults.dest, "Work note", b"second").await;
        vaults.fake.side("source", |side| fail(side, true));

        pull(&vaults, Filter::default()).await.unwrap_err();
        assert_eq!(vaults.fake.deletes("source"), no_writes());
        let kept = file_ids(&vaults.source, "Work note").await;
        assert!(kept.len() == 2 && kept.contains(&old[0]), "{kept:?}");
        assert_eq!(file(&vaults.source, "Work note", &old[0]).await, b"first");
        vaults.fake.side("source", |side| fail(side, false));

        let refused = run(&vaults, &MirrorOptions::default()).await.unwrap_err();
        assert!(
            format!("{:#}", refused.source).contains("--pull"),
            "{refused:?}"
        );
        assert_eq!(vaults.fake.writes("dest"), no_writes());

        pull(&vaults, Filter::default()).await.unwrap();
        assert_eq!(files(&vaults.source, "Work note").await, [b"second"]);
    }
}

/// A run that fails to upload a file deletes nothing: neither the copy's old
/// file nor the new one, which may have arrived. The next run replaces the
/// old file once uploads work again.
#[tokio::test]
async fn a_failed_upload_keeps_the_copys_old_file() {
    for fail in upload_failures() {
        let vaults = mirrored().await;
        let old = file_ids(&vaults.dest, "Work note").await;
        replace_file(&vaults.source, "Work note", b"second").await;
        vaults.fake.side("dest", |side| fail(side, true));
        let options = MirrorOptions {
            verify_attachments: false,
            ..MirrorOptions::default()
        };

        run(&vaults, &options).await.unwrap_err();
        assert_eq!(vaults.fake.deletes("dest"), no_writes());
        let kept = file_ids(&vaults.dest, "Work note").await;
        assert!(kept.len() == 2 && kept.contains(&old[0]), "{kept:?}");
        assert_eq!(file(&vaults.dest, "Work note", &old[0]).await, b"first");
        vaults.fake.side("dest", |side| fail(side, false));

        run(&vaults, &options).await.unwrap();
        assert_eq!(files(&vaults.dest, "Work note").await, [b"second"]);
    }
}

/// A server that gives a new file the id of one the item already has gets no
/// upload, which would overwrite that file.
#[tokio::test]
async fn a_new_file_never_goes_to_the_id_of_an_old_one() {
    let vaults = mirrored().await;
    let old = file_ids(&vaults.dest, "Work note").await;
    replace_file(&vaults.source, "Work note", b"second").await;
    vaults
        .fake
        .side("dest", |side| side.reusing_attachment_ids = true);

    let error = run(&vaults, &MirrorOptions::default()).await.unwrap_err();
    assert!(format!("{:#}", error.source).contains(&old[0]), "{error:?}");
    assert_eq!(file_ids(&vaults.dest, "Work note").await, old);
    assert_eq!(file(&vaults.dest, "Work note", &old[0]).await, b"first");
    vaults
        .fake
        .side("dest", |side| side.reusing_attachment_ids = false);

    run(&vaults, &MirrorOptions::default()).await.unwrap();
    assert_eq!(files(&vaults.dest, "Work note").await, [b"second"]);
}

/// A replacement file is read back before the file it replaces goes, also
/// with `VERIFY_ATTACHMENTS` off.
#[tokio::test]
async fn a_replacement_file_is_checked_before_the_old_one_goes() {
    let vaults = mirrored().await;
    let old = file_ids(&vaults.dest, "Work note").await;
    replace_file(&vaults.source, "Work note", b"second").await;
    vaults
        .fake
        .side("dest", |side| side.corrupting_uploads = true);
    let options = MirrorOptions {
        verify_attachments: false,
        ..MirrorOptions::default()
    };

    run(&vaults, &options).await.unwrap_err();
    assert_eq!(file(&vaults.dest, "Work note", &old[0]).await, b"first");
    vaults
        .fake
        .side("dest", |side| side.corrupting_uploads = false);
    run(&vaults, &options).await.unwrap();
    assert_eq!(files(&vaults.dest, "Work note").await, [b"second"]);
}

/// A source server that accepts a write but drops part of it stops the pull
/// before the mirror copies the loss into the destination. The original keeps
/// a marker that stops a run without `--pull`.
#[tokio::test]
async fn a_pull_checks_what_the_source_kept() {
    let vaults = mirrored().await;
    edit(&vaults.dest, "Work spare", set_notes("edited")).await;
    vaults
        .fake
        .side("source", |side| side.dropping_notes = true);

    let error = pull(&vaults, Filter::default()).await.unwrap_err();
    let original = by_name(&vaults.source.load(&PERSONAL).await.unwrap(), "Work spare").clone();
    assert!(format!("{error:#}").contains(&original.id), "{error:#}");
    assert!(marker_of(&original).is_some_and(|marker| marker.files_pending()));

    let refused = run(&vaults, &MirrorOptions::default()).await.unwrap_err();
    assert!(
        format!("{:#}", refused.source).contains("--pull"),
        "{refused:?}"
    );
    assert_eq!(vaults.fake.writes("dest"), no_writes());
    let refused = run(
        &vaults,
        &MirrorOptions {
            filter: folders(&["Private"], &[]),
            ..MirrorOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        format!("{:#}", refused.source).contains("--pull"),
        "{refused:?}"
    );
    assert_eq!(vaults.fake.writes("dest"), no_writes());
    let dest = vaults.dest.load(&PERSONAL).await.unwrap();
    assert_eq!(notes(by_name(&dest, "Work spare")), Some("edited"));
}

#[tokio::test]
async fn marker_cleanup_verifies_the_content_the_source_kept() {
    let vaults = mirrored().await;
    edit(&vaults.dest, "Work note", set_notes("recovered note")).await;
    let pulled = pull(&vaults, Filter::default()).await.unwrap();
    let outcome = mirror::mirror_after_pull(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        None,
        &pull_options(Filter::default()),
        &Capture::default(),
        &pulled.cleanup_source_ids,
    )
    .await
    .unwrap();
    vaults
        .fake
        .side("source", |side| side.dropping_notes = true);
    let error = mirror::unmark(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        &outcome.cleanup_source_items,
        4,
    )
    .await
    .unwrap_err();
    let original = by_name(&vaults.source.load(&PERSONAL).await.unwrap(), "Work note").clone();
    assert!(format!("{error:#}").contains(&original.id));
    assert_eq!(
        notes(by_name(
            &vaults.dest.load(&PERSONAL).await.unwrap(),
            "Work note"
        )),
        Some("recovered note")
    );
}

#[tokio::test]
async fn marker_cleanup_refuses_source_edits_made_after_forward_verification() {
    let vaults = mirrored().await;
    edit(&vaults.dest, "Work note", set_notes("recovered note")).await;
    let pulled = pull(&vaults, Filter::default()).await.unwrap();
    let outcome = mirror::mirror_after_pull(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        None,
        &pull_options(Filter::default()),
        &Capture::default(),
        &pulled.cleanup_source_ids,
    )
    .await
    .unwrap();
    edit(
        &vaults.source,
        "Work note",
        set_notes("new unbacked source edit"),
    )
    .await;
    vaults.fake.forget_writes();
    vaults
        .fake
        .side("source", |side| side.dropping_notes = true);
    assert!(
        mirror::unmark(
            &personal(&vaults.source),
            &personal(&vaults.dest),
            &outcome.cleanup_source_items,
            4,
        )
        .await
        .is_err()
    );
    assert_eq!(vaults.fake.writes("source"), no_writes());
    assert_eq!(
        notes(by_name(
            &vaults.source.load(&PERSONAL).await.unwrap(),
            "Work note"
        )),
        Some("new unbacked source edit")
    );
}

fn app_config(vaults: &Vaults, backups: &std::path::Path) -> vault_mirror::config::Config {
    let mut env = HashMap::new();
    for (key, value) in [
        ("BACKUP_ENABLED", "false"),
        ("SOURCE_CLIENT_ID", "user.source"),
        ("DEST_CLIENT_ID", "user.dest"),
        ("SOURCE_CLIENT_SECRET", "fake secret"),
        ("DEST_CLIENT_SECRET", "fake secret"),
        ("SOURCE_PASSWORD", PASSWORD),
        ("DEST_PASSWORD", PASSWORD),
    ] {
        env.insert(key.into(), value.into());
    }
    for (key, side) in [("SOURCE_SERVER", "source"), ("DEST_SERVER", "dest")] {
        env.insert(
            key.into(),
            format!("http://127.0.0.1:{}/{side}", vaults.fake.port),
        );
    }
    env.insert("BACKUPS_DIR".into(), backups.to_str().unwrap().into());
    vault_mirror::config::Config::from_env(&env).unwrap()
}

#[tokio::test]
async fn failed_marker_cleanup_keeps_a_durable_hold_without_source_markers() {
    use vault_mirror::app::{RunFlags, run_once};
    for fault in [
        "notes",
        "folder",
        "files",
        "file-bytes",
        "trash",
        "archive",
        "ambiguous",
    ] {
        let vaults = mirrored().await;
        edit(&vaults.dest, "Work note", set_notes("recovered note")).await;
        vaults
            .fake
            .side("source", |side| side.cleanup_fault = Some(fault));
        let backups = tempfile::tempdir().unwrap();
        let mut config = app_config(&vaults, backups.path());
        let status = run_once(
            &config,
            RunFlags {
                pull: true,
                ..RunFlags::default()
            },
            &Capture::default(),
        )
        .await;
        assert!(!status.succeeded(), "{fault}: {status:?}");
        assert_eq!(
            status.failure_stage.as_deref(),
            Some("removing markers from the source"),
            "{fault}: {status:?}"
        );
        assert!(
            backups.path().join("pull-cleanup-hold.json").is_file(),
            "{fault}"
        );
        let source = vaults.source.load(&PERSONAL).await.unwrap();
        let original = source
            .items
            .iter()
            .find(|item| item.name() == "Work note")
            .unwrap();
        assert!(marker_of(original).is_none(), "{fault}");
        assert_eq!(
            notes(by_name(
                &vaults.dest.load(&PERSONAL).await.unwrap(),
                "Work note"
            )),
            Some("recovered note")
        );
        assert_eq!(files(&vaults.dest, "Work note").await, [b"first"]);
        vaults.fake.forget_writes();
        config.filter = folders(&["Private"], &[]);
        let status = run_once(&config, RunFlags::default(), &Capture::default()).await;
        assert_eq!(
            status.failure_stage.as_deref(),
            Some("recovery hold"),
            "{fault}: {status:?}"
        );
        assert!(status.error.unwrap().contains("pull-cleanup-hold.json"));
        assert_eq!(vaults.fake.writes("source"), no_writes());
        assert_eq!(vaults.fake.writes("dest"), no_writes());
    }
}

#[tokio::test]
async fn verified_marker_cleanup_releases_its_hold() {
    use vault_mirror::app::{RunFlags, run_once};
    let vaults = mirrored().await;
    edit(&vaults.dest, "Work note", set_notes("recovered note")).await;
    let other = edit(
        &vaults.dest,
        "Private note",
        set_notes("another recovered note"),
    )
    .await;
    vaults
        .dest
        .upload_attachment(&PERSONAL, &other, "other.txt", b"different bytes")
        .await
        .unwrap();
    let backups = tempfile::tempdir().unwrap();
    let config = app_config(&vaults, backups.path());
    let status = run_once(
        &config,
        RunFlags {
            pull: true,
            ..RunFlags::default()
        },
        &Capture::default(),
    )
    .await;
    assert!(status.succeeded(), "{status:?}");
    assert!(!backups.path().join("pull-cleanup-hold.json").exists());
    assert!(
        run_once(&config, RunFlags::default(), &Capture::default())
            .await
            .succeeded()
    );
}

#[tokio::test]
async fn marker_cleanup_checks_membership_from_the_full_organization_read() {
    use vault_mirror::app::{RunFlags, run_once};
    let fake = Fake::start();
    let source = fake.open("source").await;
    let dest = fake.open("dest").await;
    let source_scope = Scope::Organization("org-source".into());
    let dest_scope = Scope::Organization("org-dest".into());
    let collection = source
        .create_collection("org-source", "Shared", None)
        .await
        .unwrap();
    source
        .create_item(
            &source_scope,
            &Draft {
                content: note("Shared note"),
                folder_id: None,
                collection_ids: vec![collection.id],
            },
        )
        .await
        .unwrap();
    mirror::mirror(
        &Endpoint {
            account: &source,
            scope: source_scope.clone(),
        },
        &Endpoint {
            account: &dest,
            scope: dest_scope.clone(),
        },
        &MirrorOptions::default(),
        &Capture::default(),
    )
    .await
    .unwrap();
    let vault = dest.load(&dest_scope).await.unwrap();
    let item = by_name(&vault, "Shared note");
    let mut draft = Draft {
        content: item.content.clone(),
        folder_id: None,
        collection_ids: item.collection_ids.clone(),
    };
    set_notes("recovered organization note")(&mut draft);
    dest.update_item(&dest_scope, item, &draft).await.unwrap();
    fake.side("source", |side| side.cleanup_fault = Some("collections"));
    let vaults = Vaults { fake, source, dest };
    let backups = tempfile::tempdir().unwrap();
    let mut config = app_config(&vaults, backups.path());
    config.source.organization_id = Some("org-source".into());
    config.dest.as_mut().unwrap().organization_id = Some("org-dest".into());
    let status = run_once(
        &config,
        RunFlags {
            pull: true,
            ..RunFlags::default()
        },
        &Capture::default(),
    )
    .await;
    assert!(!status.succeeded(), "{status:?}");
    assert_eq!(
        status.failure_stage.as_deref(),
        Some("removing markers from the source")
    );
    assert!(backups.path().join("pull-cleanup-hold.json").is_file());
    let source = vaults.source.load(&source_scope).await.unwrap();
    assert!(by_name(&source, "Shared note").collection_ids.is_empty());
    let dest = vaults.dest.load(&dest_scope).await.unwrap();
    assert_eq!(
        notes(by_name(&dest, "Shared note")),
        Some("recovered organization note")
    );
    assert_eq!(by_name(&dest, "Shared note").collection_ids.len(), 1);
}

#[tokio::test]
async fn restore_checks_requested_container_metadata_even_when_creation_echoes_altered_values() {
    use vault_mirror::{
        restore::{RestoreOptions, restore},
        snapshot,
    };
    for kind in ["folder-name", "collection-name", "collection-external-id"] {
        let fake = Fake::start();
        let account = fake.open("source").await;
        let organization = kind.starts_with("collection");
        if organization {
            account
                .create_collection("org-source", "Altered", None)
                .await
                .unwrap();
        } else {
            account.create_folder("Altered").await.unwrap();
        }
        fake.side("source", |side| {
            let name = if organization {
                &side.collections[0]["name"]
            } else {
                &side.folders[0]["name"]
            }
            .clone();
            side.folders.clear();
            side.collections.clear();
            if kind == "collection-external-id" {
                side.drop_collection_external_id = true;
            } else {
                side.altered_container_name = Some(name);
            }
        });
        let snapshot = snapshot::parse(&if organization {
            json!({
                "collections": [
                    {"id": "old-collection", "name": "Requested", "externalId": "requested-id"},
                ],
                "items": [],
            })
        } else {
            json!({"folders": [{"id": "old-folder", "name": "Requested"}], "items": []})
        })
        .unwrap();
        let target = Endpoint {
            account: &account,
            scope: if organization {
                Scope::Organization("org-source".into())
            } else {
                PERSONAL
            },
        };
        let error = restore(
            &target,
            &snapshot,
            None,
            &RestoreOptions {
                allow_non_empty: false,
                concurrency: 2,
            },
            &Capture::default(),
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(if organization { "collection" } else { "folder" }),
            "{kind}: {message}"
        );
        assert!(
            !message.contains("Requested")
                && !message.contains("Altered")
                && !message.contains("requested-id")
        );
    }
}

#[tokio::test]
async fn a_missing_upload_blob_keeps_old_files_and_stops_the_next_backup_before_mutation() {
    use vault_mirror::{
        app::{RunFlags, run_once},
        config::BackupConfig,
        store::Protection,
    };
    let vaults = mirrored().await;
    let old = file_ids(&vaults.dest, "Work note").await;
    replace_file(&vaults.source, "Work note", b"new source file").await;
    vaults.fake.side("dest", |side| side.failing_uploads = true);
    let backups = tempfile::tempdir().unwrap();
    let mut config = app_config(&vaults, backups.path());
    config.backup = Some(BackupConfig {
        protection: Protection::Passphrase("throwaway backup passphrase".into()),
        attachments: true,
        retention: None,
    });
    let status = run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(!status.succeeded());
    let current = vaults.dest.load(&PERSONAL).await.unwrap();
    let item = by_name(&current, "Work note");
    let allocated = item
        .attachments
        .iter()
        .find(|file| !old.contains(&file.id))
        .unwrap();
    assert!(status.error.unwrap().contains(&allocated.id));
    assert_eq!(file(&vaults.dest, "Work note", &old[0]).await, b"first");
    vaults
        .fake
        .side("dest", |side| side.failing_uploads = false);
    vaults.fake.forget_writes();
    let status = run_once(&config, RunFlags::default(), &Capture::default()).await;
    assert!(!status.succeeded());
    assert_eq!(status.failure_stage.as_deref(), Some("destination backup"));
    assert_eq!(vaults.fake.writes("source"), no_writes());
    assert_eq!(vaults.fake.writes("dest"), no_writes());
    assert_eq!(file(&vaults.dest, "Work note", &old[0]).await, b"first");
}

/// An item added in the destination that the source kept only part of is
/// written again by the next pull, instead of the source's version being kept
/// as an edit made there in between.
#[tokio::test]
async fn a_pull_writes_an_added_item_again_after_the_source_dropped_part_of_it() {
    let vaults = mirrored().await;
    let draft = Draft {
        content: note("Added in the destination"),
        folder_id: None,
        collection_ids: Vec::new(),
    };
    vaults.dest.create_item(&PERSONAL, &draft).await.unwrap();
    vaults
        .fake
        .side("source", |side| side.dropping_notes = true);
    pull(&vaults, Filter::default()).await.unwrap_err();
    let blocked = run(&vaults, &MirrorOptions::default()).await.unwrap_err();
    assert!(format!("{:#}", blocked.source).contains("--pull"));
    assert_eq!(vaults.fake.writes("dest"), no_writes());
    vaults
        .fake
        .side("source", |side| side.dropping_notes = false);

    pull(&vaults, Filter::default()).await.unwrap();
    let source = vaults.source.load(&PERSONAL).await.unwrap();
    let added = by_name(&source, "Added in the destination");
    assert_eq!(notes(added), Some("Added in the destination as written"));
}

/// Markers are taken off only the source items the pull names: here not the
/// one the filter leaves out, though an earlier pull marked it too.
#[tokio::test]
async fn unmarking_leaves_items_outside_the_pull_alone() {
    let vaults = mirrored().await;
    // What an earlier pull that was never unmarked leaves: originals that
    // name their copies.
    let source = vaults.source.load(&PERSONAL).await.unwrap();
    let dest = vaults.dest.load(&PERSONAL).await.unwrap();
    for (name, folder) in [("Work note", "Work"), ("Private note", "Private")] {
        let original = by_name(&source, name);
        let copy = by_name(&dest, name);
        let content = mark_copy(
            original,
            copy,
            Some(folder),
            &[],
            Mode::Personal,
            Some(original),
        );
        let draft = Draft {
            content,
            folder_id: original.folder_id.clone(),
            collection_ids: Vec::new(),
        };
        (vaults.source.update_item(&PERSONAL, original, &draft))
            .await
            .unwrap();
    }

    let pulled = pull(&vaults, folders(&["Work"], &[])).await.unwrap();
    let outcome = mirror::mirror_after_pull(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        None,
        &pull_options(folders(&["Work"], &[])),
        &Capture::default(),
        &pulled.cleanup_source_ids,
    )
    .await
    .unwrap();
    let unmarked = mirror::unmark(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        &outcome.cleanup_source_items,
        4,
    )
    .await
    .unwrap();
    assert_eq!(unmarked, 1);
    let source = vaults.source.load(&PERSONAL).await.unwrap();
    assert!(marker_of(by_name(&source, "Work note")).is_none());
    assert!(marker_of(by_name(&source, "Private note")).is_some());
}

/// A copy whose source item is gone goes to the destination's trash, not away
/// for good. One that changed in any way since the backup the plan comes from
/// stays where it is.
#[tokio::test]
async fn obsolete_copies_go_to_the_trash_unless_they_changed_after_the_backup() {
    for change in ["nothing", "trash", "folder", "archive", "revision", "file"] {
        let vaults = mirrored().await;
        let source = vaults.source.load(&PERSONAL).await.unwrap();
        let original = by_name(&source, "Work note").id.clone();
        vaults.source.delete_items(&[original]).await.unwrap();
        let backed_up = vaults.dest.load(&PERSONAL).await.unwrap();
        let copy = by_name(&backed_up, "Work note").id.clone();
        let private = folder_id(&backed_up, "Private");
        let stamp = json!("2026-10-17T00:00:00.000Z");
        vaults
            .fake
            .change_item("dest", &copy, |cipher| match change {
                "trash" => cipher["deletedDate"] = stamp,
                "folder" => cipher["folderId"] = json!(private),
                "archive" => cipher["archivedDate"] = stamp,
                "revision" => cipher["revisionDate"] = stamp,
                // Same id, other file.
                "file" => cipher["attachments"][0]["size"] = json!("6"),
                _ => {}
            });

        let result = mirror::mirror_onto(
            &personal(&vaults.source),
            &personal(&vaults.dest),
            Some(backed_up),
            &MirrorOptions::default(),
            &Capture::default(),
        )
        .await;
        let kept = vaults.fake.side("dest", |side| {
            (side.ciphers.iter())
                .find(|c| c["id"] == copy.as_str())
                .cloned()
        });
        let kept = kept.unwrap_or_else(|| panic!("{change}: the copy was deleted for good"));
        match change {
            "nothing" => {
                result.unwrap();
                assert!(kept["deletedDate"].is_string(), "the copy is in the trash");
            }
            _ => {
                let error = result.unwrap_err();
                assert!(
                    format!("{:#}", error.source).contains(&copy),
                    "{change}: {error:?}"
                );
            }
        }
    }
}

/// A source server that stops showing an item type, as Bitwarden's does with
/// bank accounts, driver's licences and passports behind a feature flag, makes
/// the copies of such items look like copies of deleted ones. The run stops
/// rather than delete them.
#[tokio::test]
async fn copies_of_items_the_source_stopped_showing_are_not_deleted() {
    let vaults = mirrored().await;
    let draft = Draft {
        content: passport("Passport"),
        folder_id: None,
        collection_ids: Vec::new(),
    };
    vaults.source.create_item(&PERSONAL, &draft).await.unwrap();
    run(&vaults, &MirrorOptions::default()).await.unwrap();
    let dest = vaults.dest.load(&PERSONAL).await.unwrap();
    let copy = by_name(&dest, "Passport").id.clone();
    vaults
        .fake
        .side("source", |side| side.hiding_new_types = true);

    let refused = run(&vaults, &MirrorOptions::default()).await.unwrap_err();
    assert!(
        format!("{:#}", refused.source).contains(&copy),
        "{refused:?}"
    );
    assert_eq!(vaults.fake.writes("dest"), no_writes());
}

/// A filter name that matches nothing is reported by its setting, without the
/// name: folder names are vault content, and warnings reach logs.
#[tokio::test]
async fn filter_names_that_match_nothing_stay_out_of_messages() {
    let vaults = mirrored().await;
    let capture = Capture::default();
    let options = MirrorOptions {
        filter: folders(&["Work", "Secret project"], &["Old secrets"]),
        ..MirrorOptions::default()
    };
    let outcome = mirror::mirror(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        &options,
        &capture,
    )
    .await
    .unwrap();
    let lines = capture.0.lock().unwrap().clone();
    for setting in ["INCLUDE_FOLDERS", "EXCLUDE_FOLDERS"] {
        assert!(lines.iter().any(|line| line.contains(setting)), "{lines:?}");
    }
    for line in lines.iter().chain(&outcome.warnings) {
        assert!(
            !line.contains("Secret project") && !line.contains("Old secrets"),
            "{line}"
        );
    }
}

/// A pull that stopped halfway through an organization item cannot leave it
/// out when the source server requires a collection and the item has none
/// any more: the mirror after the pull would replace it with the unfinished
/// source item.
#[tokio::test]
async fn a_pull_stops_at_an_unfinished_item_the_source_cannot_take_any_more() {
    let fake = Fake::start();
    let source = fake.open("source").await;
    let dest = fake.open("dest").await;
    let (source_scope, dest_scope) = (
        Scope::Organization("org-source".into()),
        Scope::Organization("org-dest".into()),
    );
    let shared = source
        .create_collection("org-source", "Shared", None)
        .await
        .unwrap();
    let draft = Draft {
        content: note("Shared note"),
        folder_id: None,
        collection_ids: vec![shared.id.clone()],
    };
    source.create_item(&source_scope, &draft).await.unwrap();
    let (source, dest) = (
        Endpoint {
            account: &source,
            scope: source_scope.clone(),
        },
        Endpoint {
            account: &dest,
            scope: dest_scope.clone(),
        },
    );
    mirror::mirror(
        &source,
        &dest,
        &MirrorOptions::default(),
        &Capture::default(),
    )
    .await
    .unwrap();
    // Added in the destination during the outage, in no collection, and made
    // in the source by a pull that stopped before it finished.
    let draft = Draft {
        content: note("Added in no collection"),
        folder_id: None,
        collection_ids: Vec::new(),
    };
    let added = dest.account.create_item(&dest_scope, &draft).await.unwrap();
    let draft = Draft {
        content: mark_copy(&added, &added, None, &[], Mode::Organization, None),
        folder_id: None,
        collection_ids: vec![shared.id.clone()],
    };
    source
        .account
        .create_item(&source_scope, &draft)
        .await
        .unwrap();
    let mut edited = draft;
    edited.content = added.content.clone();
    edited
        .content
        .insert("notes".into(), Node::secret("edited since"));
    edited.collection_ids = Vec::new();
    dest.account
        .update_item(&dest_scope, &added, &edited)
        .await
        .unwrap();
    fake.side("source", |side| side.hiding_new_types = true);

    let source_vault = source.account.load(&source_scope).await.unwrap();
    let dest_vault = dest.account.load(&dest_scope).await.unwrap();
    fake.forget_writes();
    let options = pull_options(Filter::default());
    let error = mirror::pull(
        &source,
        &dest,
        &source_vault,
        &dest_vault,
        &options,
        true,
        &Capture::default(),
    )
    .await
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(&format!("which the source server requires ({})", added.id)),
        "{message}"
    );
    assert_eq!(fake.writes("source"), no_writes());
}

/// An organization item added in the destination comes back in the
/// collections the filter selects, and only those. The pull creates no
/// collection the filter leaves out.
#[tokio::test]
async fn a_filtered_pull_adds_organization_items_only_to_selected_collections() {
    let fake = Fake::start();
    let source = fake.open("source").await;
    let dest = fake.open("dest").await;
    let (source_scope, dest_scope) = (
        Scope::Organization("org-source".into()),
        Scope::Organization("org-dest".into()),
    );
    let shared = source
        .create_collection("org-source", "Shared", None)
        .await
        .unwrap();
    let draft = Draft {
        content: note("Shared note"),
        folder_id: None,
        collection_ids: vec![shared.id.clone()],
    };
    source.create_item(&source_scope, &draft).await.unwrap();
    let (source, dest) = (
        Endpoint {
            account: &source,
            scope: source_scope.clone(),
        },
        Endpoint {
            account: &dest,
            scope: dest_scope.clone(),
        },
    );
    mirror::mirror(
        &source,
        &dest,
        &MirrorOptions::default(),
        &Capture::default(),
    )
    .await
    .unwrap();
    let dest_vault = dest.account.load(&dest_scope).await.unwrap();
    let other = dest
        .account
        .create_collection("org-dest", "Other", None)
        .await
        .unwrap();
    let both = vec![dest_vault.collections[0].id.clone(), other.id];
    let draft = Draft {
        content: note("Added in two collections"),
        folder_id: None,
        collection_ids: both,
    };
    dest.account.create_item(&dest_scope, &draft).await.unwrap();

    let source_vault = source.account.load(&source_scope).await.unwrap();
    let dest_vault = dest.account.load(&dest_scope).await.unwrap();
    fake.forget_writes();
    let options = pull_options(Filter {
        include: vec!["Shared".into()],
        exclude: Vec::new(),
    });
    let pulled = mirror::pull(
        &source,
        &dest,
        &source_vault,
        &dest_vault,
        &options,
        true,
        &Capture::default(),
    )
    .await
    .unwrap();
    assert_eq!(pulled.summary.items_created, 1);
    let writes = fake.writes("source");
    assert!(
        !writes.iter().any(|w| w.ends_with("/collections")),
        "{writes:?}"
    );
    let after = source.account.load(&source_scope).await.unwrap();
    let added = by_name(&after, "Added in two collections");
    assert_eq!(added.collection_ids, [shared.id]);
}

#[tokio::test]
async fn a_pull_does_not_accept_a_file_dropped_by_the_final_item_write() {
    let vaults = mirrored().await;
    replace_file(&vaults.dest, "Work note", b"destination replacement").await;
    vaults
        .fake
        .side("source", |side| side.dropping_files_after_upload = true);
    assert!(pull(&vaults, Filter::default()).await.is_err());
    assert_eq!(
        files(&vaults.dest, "Work note").await,
        [b"destination replacement"]
    );
    assert!(run(&vaults, &MirrorOptions::default()).await.is_err());
    assert_eq!(vaults.fake.writes("dest"), no_writes());
}

#[tokio::test]
async fn a_pull_rejects_an_item_the_source_puts_in_the_trash() {
    let vaults = mirrored().await;
    edit(&vaults.dest, "Work spare", set_notes("recovered note")).await;
    vaults
        .fake
        .side("source", |side| side.trashing_writes = true);
    assert!(pull(&vaults, Filter::default()).await.is_err());
    assert_eq!(
        notes(by_name(
            &vaults.dest.load(&PERSONAL).await.unwrap(),
            "Work spare"
        )),
        Some("recovered note")
    );
    assert!(run(&vaults, &MirrorOptions::default()).await.is_err());
    assert_eq!(vaults.fake.writes("dest"), no_writes());
}

#[tokio::test]
async fn a_pull_rejects_ambiguous_pending_source_items_before_writing() {
    let vaults = mirrored().await;
    let added = vaults
        .dest
        .create_item(
            &PERSONAL,
            &Draft {
                content: note("Added during the outage"),
                folder_id: None,
                collection_ids: Vec::new(),
            },
        )
        .await
        .unwrap();
    let draft = Draft {
        content: mark_copy(&added, &added, None, &[], Mode::Personal, None),
        folder_id: None,
        collection_ids: Vec::new(),
    };
    let first = vaults.source.create_item(&PERSONAL, &draft).await.unwrap();
    let second = vaults.source.create_item(&PERSONAL, &draft).await.unwrap();
    let error = pull(&vaults, Filter::default()).await.unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(&first.id) && message.contains(&second.id));
    assert_eq!(vaults.fake.writes("source"), no_writes());
    assert_eq!(vaults.fake.writes("dest"), no_writes());
}

#[tokio::test]
async fn a_verified_pull_keeps_source_files_and_destination_notes_then_can_resume() {
    let vaults = mirrored().await;
    replace_file(&vaults.source, "Work note", b"source replacement").await;
    edit(&vaults.dest, "Work note", set_notes("destination note")).await;
    let pulled = pull(&vaults, Filter::default()).await.unwrap();
    assert!(run(&vaults, &MirrorOptions::default()).await.is_err());
    mirror::mirror_after_pull(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        None,
        &pull_options(Filter::default()),
        &Capture::default(),
        &pulled.cleanup_source_ids,
    )
    .await
    .unwrap();
    assert_eq!(
        files(&vaults.dest, "Work note").await,
        [b"source replacement"]
    );
    assert_eq!(
        notes(by_name(
            &vaults.dest.load(&PERSONAL).await.unwrap(),
            "Work note"
        )),
        Some("destination note")
    );
    let resumed = pull(&vaults, Filter::default()).await.unwrap();
    let outcome = mirror::mirror_after_pull(
        &personal(&vaults.source),
        &personal(&vaults.dest),
        None,
        &pull_options(Filter::default()),
        &Capture::default(),
        &resumed.cleanup_source_ids,
    )
    .await
    .unwrap();
    assert_eq!(
        mirror::unmark(
            &personal(&vaults.source),
            &personal(&vaults.dest),
            &outcome.cleanup_source_items,
            4
        )
        .await
        .unwrap(),
        1
    );
    run(&vaults, &MirrorOptions::default()).await.unwrap();
}

#[tokio::test]
async fn a_pull_rejects_collection_membership_the_source_dropped() {
    let fake = Fake::start();
    let source = fake.open("source").await;
    let dest = fake.open("dest").await;
    let collection = dest
        .create_collection("org-dest", "Shared", None)
        .await
        .unwrap();
    dest.create_item(
        &Scope::Organization("org-dest".into()),
        &Draft {
            content: note("Added during the outage"),
            folder_id: None,
            collection_ids: vec![collection.id],
        },
    )
    .await
    .unwrap();
    let source = Endpoint {
        account: &source,
        scope: Scope::Organization("org-source".into()),
    };
    let dest = Endpoint {
        account: &dest,
        scope: Scope::Organization("org-dest".into()),
    };
    let source_vault = source.account.load(&source.scope).await.unwrap();
    let dest_vault = dest.account.load(&dest.scope).await.unwrap();
    fake.side("source", |side| side.dropping_collections = true);
    fake.forget_writes();
    let error = mirror::pull(
        &source,
        &dest,
        &source_vault,
        &dest_vault,
        &pull_options(Filter::default()),
        true,
        &Capture::default(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("only part"), "{error:#}");
    assert_eq!(fake.writes("dest"), no_writes());
    assert!(
        source
            .account
            .load(&source.scope)
            .await
            .unwrap()
            .items
            .iter()
            .all(|item| marker_of(item).is_some_and(|marker| marker.files_pending()))
    );

    fake.side("source", |side| side.dropping_collections = false);
    let source_vault = source.account.load(&source.scope).await.unwrap();
    mirror::pull(
        &source,
        &dest,
        &source_vault,
        &dest_vault,
        &pull_options(Filter::default()),
        true,
        &Capture::default(),
    )
    .await
    .unwrap();
    let recovered = source.account.load(&source.scope).await.unwrap();
    assert_eq!(recovered.items.len(), 1);
    let shared = recovered
        .collections
        .iter()
        .find(|collection| collection.name == "Shared")
        .unwrap();
    assert_eq!(
        by_name(&recovered, "Added during the outage")
            .collection_ids
            .as_slice(),
        std::slice::from_ref(&shared.id)
    );
}
