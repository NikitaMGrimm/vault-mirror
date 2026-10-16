//! Restores against a small local stand-in for a Bitwarden server that can
//! drop or replace what it is sent. The sandbox servers never misbehave, so
//! these cases need a server that does on purpose.

mod common;

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::Result;
use common::Capture;
use serde_json::{Value, json};
use time::OffsetDateTime;
use vault_mirror::{
    api::{ApiCredentials, ClientOptions, ServerUrls, build_http_client},
    crypto::{KdfParams, MasterSecret, SymKey},
    mirror::Endpoint,
    node::Node,
    restore::{RestoreOptions, RestoreReport, restore},
    snapshot::{self, AttachmentStorage, Restorable, RestorableItem, SnapshotAttachment},
    store::{Keys, Protection, Store, VaultOrigin},
    vault::{Account, Draft, Scope},
};

const PASSWORD: &str = "throwaway fixture password";
const EMAIL: &str = "fixture@example.com";
const ITERATIONS: u32 = 600_000;

#[derive(Clone, Copy, Default)]
struct Faults {
    /// Stores and returns items without their notes.
    drop_notes: bool,
    /// Accepts attachment uploads but never lists them.
    hide_uploads: bool,
    /// Keeps a different, validly encrypted file in place of each upload.
    replace_uploads: bool,
    /// Says it is Bitwarden's server, which hides bank accounts.
    bitwarden: bool,
    /// Has no premium subscription, so attachments are refused.
    free: bool,
    reset_reprompt: bool,
}

struct State {
    faults: Faults,
    user_key: SymKey,
    profile: Value,
    folders: Vec<Value>,
    ciphers: Vec<Value>,
    /// Wrapped attachment keys by attachment id.
    attachment_keys: HashMap<String, String>,
    blobs: HashMap<String, Vec<u8>>,
    /// Every request that changes something, as method and path.
    writes: Vec<String>,
    next_id: usize,
}

struct Server {
    url: String,
    state: Arc<Mutex<State>>,
}

impl Server {
    fn start(faults: Faults) -> Self {
        let master = MasterSecret::derive(
            PASSWORD,
            EMAIL,
            &KdfParams::Pbkdf2 {
                iterations: ITERATIONS,
            },
        )
        .unwrap();
        let keys = master.new_account_keys().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            faults,
            user_key: keys.user_key,
            profile: json!({
                "id": "fixture-user",
                "email": EMAIL,
                "key": keys.wrapped_user_key,
                "organizations": [],
                "premium": !faults.free,
            }),
            folders: Vec::new(),
            ciphers: Vec::new(),
            attachment_keys: HashMap::new(),
            blobs: HashMap::new(),
            writes: Vec::new(),
            next_id: 0,
        }));
        let shared = state.clone();
        let base = url.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some((method, path, body)) = read_request(&mut stream) else {
                    continue;
                };
                let (status, body) = respond(&shared, &base, &method, &path, &body);
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Self { url, state }
    }

    async fn open(&self) -> Account {
        Account::open(
            build_http_client(&ClientOptions::default()).unwrap(),
            ServerUrls::from_server(&self.url).unwrap(),
            ApiCredentials {
                client_id: "fixture".into(),
                client_secret: "fixture".into(),
                device_id: "fixture".into(),
            },
            PASSWORD,
        )
        .await
        .unwrap()
    }

    fn writes(&self) -> Vec<String> {
        self.state.lock().unwrap().writes.clone()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut data = Vec::new();
    let mut chunk = [0; 8192];
    let (head_end, length) = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..read]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while data.len() < head_end + length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&data[..head_end]).to_string();
    let mut first = head.lines().next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    Some((method, path, data[head_end..head_end + length].to_vec()))
}

fn respond(
    state: &Mutex<State>,
    base: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let mut state = state.lock().unwrap();
    let json = || serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let segments: Vec<&str> = path
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_start_matches('/')
        .split('/')
        .collect();
    if method != "GET" && path != "/identity/connect/token" {
        state.writes.push(format!("{method} {path}"));
    }
    let reply = match (method, segments.as_slice()) {
        ("POST", ["identity", "connect", "token"]) => {
            json!({"access_token": "fixture-token", "Kdf": 0, "KdfIterations": ITERATIONS})
        }
        ("GET", ["api", "accounts", "profile"]) => state.profile.clone(),
        ("GET", ["api", "config"]) => match state.faults.bitwarden {
            true => json!({"server": null, "featureStates": {}}),
            false => json!({"server": {"name": "Vaultwarden"}}),
        },
        ("GET", ["api", "sync"]) => json!({
            "folders": state.folders,
            "ciphers": state.ciphers,
            "collections": [],
        }),
        ("POST", ["api", "folders"]) => {
            let mut folder = json();
            folder["id"] = json!(next_id(&mut state, "folder"));
            state.folders.push(folder.clone());
            folder
        }
        ("POST", ["api", "ciphers"]) => {
            let mut cipher = json();
            let content = cipher.as_object_mut().unwrap();
            content.entry("reprompt").or_insert(json!(0));
            content.entry("favorite").or_insert(json!(false));
            if state.faults.reset_reprompt {
                cipher["reprompt"] = json!(0);
            }
            cipher["id"] = json!(next_id(&mut state, "item"));
            cipher["revisionDate"] = json!("2026-10-16T12:00:00.000Z");
            cipher["creationDate"] = json!("2026-10-16T12:00:00.000Z");
            cipher["deletedDate"] = Value::Null;
            cipher["archivedDate"] = Value::Null;
            cipher["collectionIds"] = json!([]);
            cipher["attachments"] = json!([]);
            if state.faults.drop_notes {
                cipher.as_object_mut().unwrap().remove("notes");
            }
            state.ciphers.push(cipher.clone());
            cipher
        }
        ("POST", ["api", "ciphers", item, "attachment", "v2"]) => {
            let request = json();
            let id = next_id(&mut state, "attachment");
            let key = request["key"].as_str().unwrap_or_default().to_string();
            state.attachment_keys.insert(id.clone(), key.clone());
            if !state.faults.hide_uploads {
                let cipher = cipher_mut(&mut state, item);
                cipher["attachments"].as_array_mut().unwrap().push(json!({
                    "id": id,
                    "fileName": request["fileName"],
                    "size": request["fileSize"],
                    "key": key,
                }));
            }
            json!({
                "attachmentId": id,
                "fileUploadType": 1,
                "url": format!("{base}/blob/{id}"),
            })
        }
        ("PUT", ["blob", id]) => {
            let mut blob = body.to_vec();
            if state.faults.replace_uploads {
                let wrapped = &state.attachment_keys[*id];
                let key = state.user_key.unwrap_key(wrapped).unwrap();
                blob = key.encrypt_blob(b"not what was uploaded").unwrap();
            }
            state.blobs.insert(id.to_string(), blob);
            Value::Null
        }
        ("GET", ["api", "ciphers", _, "attachment", id]) => {
            json!({ "url": format!("{base}/blob/{id}") })
        }
        ("GET", ["blob", id]) => match state.blobs.get(*id) {
            Some(blob) => return (200, blob.clone()),
            None => json!(null),
        },
        ("PUT", ["api", "ciphers", "archive"]) => {
            for id in json()["ids"].as_array().cloned().unwrap_or_default() {
                cipher_mut(&mut state, id.as_str().unwrap())["archivedDate"] =
                    json!("2026-10-16T12:00:01.000Z");
            }
            Value::Null
        }
        ("DELETE", ["api", "ciphers", item, "attachment", id]) => {
            let cipher = cipher_mut(&mut state, item);
            cipher["attachments"]
                .as_array_mut()
                .unwrap()
                .retain(|a| a["id"] != *id);
            Value::Null
        }
        _ => return (404, br#"{"message":"not found"}"#.to_vec()),
    };
    (200, reply.to_string().into_bytes())
}

fn next_id(state: &mut State, kind: &str) -> String {
    state.next_id += 1;
    format!("{kind}-{}", state.next_id)
}

fn cipher_mut<'a>(state: &'a mut State, id: &str) -> &'a mut Value {
    state
        .ciphers
        .iter_mut()
        .find(|cipher| cipher["id"] == id)
        .expect("a known item")
}

/// A secure note to back up, with its attachment files by name.
struct Note {
    item_type: u64,
    notes: &'static str,
    archived: bool,
    files: Vec<(&'static str, &'static [u8])>,
}

fn note(notes: &'static str, files: &[(&'static str, &'static [u8])]) -> Note {
    Note {
        item_type: 2,
        notes,
        archived: false,
        files: files.to_vec(),
    }
}

fn keys(dir: &Path) -> Keys {
    Keys::open(dir, &Protection::Passphrase("fixture passphrase".into())).unwrap()
}

/// Saves `notes` in a vault on a fixture server and backs that vault up into
/// `store`, with attachment content when `attachments` is set. Returns the
/// snapshot as restore reads it, and its attachment records.
async fn backed_up(
    store: &Store<'_>,
    notes: &[Note],
    attachments: bool,
) -> (Restorable, Vec<SnapshotAttachment>) {
    let source = Server::start(Faults::default());
    let account = source.open().await;
    let folder = account.create_folder("Work").await.unwrap();
    for (index, note) in notes.iter().enumerate() {
        let content = [
            ("type".to_string(), Node::Number(note.item_type.into())),
            ("name".to_string(), Node::secret(format!("Note {index}"))),
            ("notes".to_string(), Node::secret(note.notes)),
            (
                "secureNote".to_string(),
                Node::Map([("type".to_string(), Node::Number(0.into()))].into()),
            ),
        ];
        let draft = Draft {
            content: content.into(),
            folder_id: Some(folder.id.clone()),
            collection_ids: Vec::new(),
        };
        let item = account.create_item(&Scope::Personal, &draft).await.unwrap();
        if note.archived {
            account.set_archived(&[item.id], true).await.unwrap();
        }
    }
    let vault = account.load(&Scope::Personal).await.unwrap();
    let origin = VaultOrigin {
        api_url: account.api_url().to_string(),
        account_id: account.user_id.clone(),
        organization_id: None,
    };
    let writer = store
        .begin_snapshot(&origin, OffsetDateTime::now_utc())
        .unwrap();
    let mut records = Vec::new();
    for (item, note) in vault.items.iter().zip(notes) {
        for (index, (file_name, data)) in note.files.iter().enumerate() {
            let attachment_id = format!("{}-file-{index}", item.id);
            let storage = match attachments {
                true => writer.write_attachment(&attachment_id, data).unwrap(),
                false => AttachmentStorage::Omitted,
            };
            records.push(SnapshotAttachment {
                item_id: item.id.clone(),
                attachment_id,
                file_name: file_name.to_string(),
                size: data.len() as u64 + 100,
                storage,
            });
        }
    }
    let document = snapshot::build(&vault, &Scope::Personal, &records).unwrap();
    let path = writer.finish(document).unwrap();
    let restorable = snapshot::parse(&store.read_snapshot(&path).unwrap()).unwrap();
    (restorable, records)
}

fn file_of(backups: &Path, record: &SnapshotAttachment) -> PathBuf {
    let AttachmentStorage::Snapshot { snapshot_id, .. } = &record.storage else {
        panic!("a file stored with its snapshot")
    };
    (backups.join("source/attachments").join(snapshot_id))
        .join(format!("{}.age", record.attachment_id))
}

fn corrupt(path: &Path) {
    let mut data = std::fs::read(path).unwrap();
    data.truncate(data.len() - 1);
    std::fs::write(path, data).unwrap();
}

async fn run(
    server: &Server,
    snapshot: &Restorable,
    store: Option<&Store<'_>>,
    capture: &Capture,
) -> Result<RestoreReport> {
    let account = server.open().await;
    let target = Endpoint {
        account: &account,
        scope: Scope::Personal,
    };
    let options = RestoreOptions {
        allow_non_empty: false,
        concurrency: 2,
    };
    restore(&target, snapshot, store, &options, capture).await
}

#[tokio::test]
async fn a_restore_handles_omitted_client_flags_and_preserves_explicit_values() {
    for reprompt in [None, Some(0), Some(1)] {
        let mut content = [
            ("type".to_string(), Node::Number(2.into())),
            ("name".to_string(), Node::secret("Restored note")),
            (
                "secureNote".to_string(),
                Node::Map([("type".to_string(), Node::Number(0.into()))].into()),
            ),
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
        if let Some(value) = reprompt {
            content.insert("reprompt".into(), Node::Number(value.into()));
            content.insert("favorite".into(), Node::Bool(value != 0));
        }
        let snapshot = Restorable {
            items: vec![RestorableItem {
                id: "saved-item".into(),
                folder_id: None,
                collection_ids: Vec::new(),
                archived: false,
                content,
                attachments: Vec::new(),
            }],
            ..Restorable::default()
        };
        let server = Server::start(Faults::default());
        let report = run(&server, &snapshot, None, &Capture::default())
            .await
            .unwrap();
        assert_eq!(report.items, 1);
        let state = server.state.lock().unwrap();
        assert_eq!(state.ciphers[0]["reprompt"], json!(reprompt.unwrap_or(0)));
        assert_eq!(state.ciphers[0]["favorite"], json!(reprompt == Some(1)));
    }
}

#[tokio::test]
async fn a_restore_refuses_a_lost_master_password_reprompt() {
    let snapshot = Restorable {
        items: vec![RestorableItem {
            id: "saved-item".into(),
            folder_id: None,
            collection_ids: Vec::new(),
            archived: false,
            content: [
                ("type".to_string(), Node::Number(2.into())),
                ("name".to_string(), Node::secret("Restored note")),
                ("reprompt".to_string(), Node::Number(1.into())),
                ("favorite".to_string(), Node::Bool(true)),
            ]
            .into(),
            attachments: Vec::new(),
        }],
        ..Restorable::default()
    };
    let server = Server::start(Faults {
        reset_reprompt: true,
        ..Faults::default()
    });
    let error = run(&server, &snapshot, None, &Capture::default())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("content.reprompt"),
        "{error:#}"
    );
    assert_eq!(server.state.lock().unwrap().ciphers.len(), 1);
}

#[tokio::test]
async fn a_restore_that_the_server_stores_without_notes_fails() {
    let dir = tempfile::tempdir().unwrap();
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let (snapshot, _) = backed_up(&store, &[note("first", &[]), note("second", &[])], true).await;
    let server = Server::start(Faults {
        drop_notes: true,
        ..Faults::default()
    });
    let error = run(&server, &snapshot, Some(&store), &Capture::default())
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("content.notes"), "{message}");
    assert!(!message.contains("first") && !message.contains("second"));
    // What was written stays for inspection.
    let writes = server.writes();
    assert!(writes.iter().all(|write| !write.starts_with("DELETE")));
    assert_eq!(server.state.lock().unwrap().ciphers.len(), 2);
}

#[tokio::test]
async fn a_restore_fails_when_the_server_keeps_a_different_file() {
    let dir = tempfile::tempdir().unwrap();
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"the only copy")];
    let (snapshot, _) = backed_up(&store, &[note("n", files)], true).await;
    let server = Server::start(Faults {
        replace_uploads: true,
        ..Faults::default()
    });
    let error = run(&server, &snapshot, Some(&store), &Capture::default())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("other content"), "{error:#}");
}

#[tokio::test]
async fn a_restore_fails_when_the_server_drops_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"the only copy")];
    let (snapshot, _) = backed_up(&store, &[note("n", files)], true).await;
    let server = Server::start(Faults {
        hide_uploads: true,
        ..Faults::default()
    });
    let error = run(&server, &snapshot, Some(&store), &Capture::default())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("attachments differ"),
        "{error:#}"
    );
}

#[tokio::test]
async fn a_missing_attachment_file_stops_the_restore_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"the only copy")];
    let (snapshot, records) = backed_up(&store, &[note("n", &[]), note("n", files)], true).await;
    let server = Server::start(Faults::default());
    // Without the backup, as with a backup that lost the file.
    assert!(
        run(&server, &snapshot, None, &Capture::default())
            .await
            .is_err()
    );
    std::fs::remove_file(file_of(dir.path(), &records[0])).unwrap();
    assert!(
        run(&server, &snapshot, Some(&store), &Capture::default())
            .await
            .is_err()
    );
    assert_eq!(server.writes(), Vec::<String>::new());
}

#[tokio::test]
async fn a_corrupt_attachment_file_stops_the_restore_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"the only copy")];
    let (snapshot, records) = backed_up(&store, &[note("n", &[]), note("n", files)], true).await;
    corrupt(&file_of(dir.path(), &records[0]));
    let server = Server::start(Faults::default());
    assert!(
        run(&server, &snapshot, Some(&store), &Capture::default())
            .await
            .is_err()
    );
    assert_eq!(server.writes(), Vec::<String>::new());
}

/// A damaged file stops the restore even when the target would not get it,
/// because it hides the item or because the account may not upload files.
#[tokio::test]
async fn a_damaged_file_stops_the_restore_even_where_the_target_would_leave_it_out() {
    let files: &[(&str, &[u8])] = &[("a.txt", b"the only copy")];
    let mut bank_account = note("n", files);
    bank_account.item_type = 6;
    let hiding = Faults {
        bitwarden: true,
        ..Faults::default()
    };
    let free = Faults {
        free: true,
        ..Faults::default()
    };
    for (faults, notes) in [
        (hiding, vec![note("n", &[]), bank_account]),
        (free, vec![note("n", &[]), note("n", files)]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let keys = keys(dir.path());
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let (snapshot, records) = backed_up(&store, &notes, true).await;
        let intact = Server::start(faults);
        let report = run(&intact, &snapshot, Some(&store), &Capture::default())
            .await
            .unwrap();
        assert_eq!(report.items_left_out + report.attachments_skipped, 1);

        corrupt(&file_of(dir.path(), &records[0]));
        let server = Server::start(faults);
        assert!(
            run(&server, &snapshot, Some(&store), &Capture::default())
                .await
                .is_err()
        );
        assert_eq!(server.writes(), Vec::<String>::new());
    }
}

#[tokio::test]
async fn files_with_the_same_name_restore_separately_and_are_verified() {
    let dir = tempfile::tempdir().unwrap();
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let files: &[(&str, &[u8])] = &[("scan.pdf", b"first scan"), ("scan.pdf", b"second scan")];
    let mut archived = note("n", files);
    archived.archived = true;
    let (snapshot, _) = backed_up(&store, &[archived, note("other", &[])], true).await;
    let server = Server::start(Faults::default());
    let report = run(&server, &snapshot, Some(&store), &Capture::default())
        .await
        .unwrap();
    assert_eq!(
        (
            report.items,
            report.attachments_restored,
            report.folders_created
        ),
        (2, 2, 1)
    );
    assert!(!report.warned());

    let account = server.open().await;
    let vault = account.load(&Scope::Personal).await.unwrap();
    let item = vault
        .items
        .iter()
        .find(|item| item.name() == "Note 0")
        .unwrap();
    assert!(item.archived);
    let mut restored = Vec::new();
    for attachment in &item.attachments {
        assert_eq!(attachment.file_name, "scan.pdf");
        restored.push(
            account
                .download_attachment(&Scope::Personal, item, attachment)
                .await
                .unwrap(),
        );
    }
    restored.sort();
    assert_eq!(restored, [b"first scan".to_vec(), b"second scan".to_vec()]);
}

#[tokio::test]
async fn a_backup_without_attachment_content_restores_the_items_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let keys = keys(dir.path());
    let store = Store::open(dir.path(), "source", &keys).unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"left out")];
    let (snapshot, _) = backed_up(&store, &[note("n", files)], false).await;
    let server = Server::start(Faults::default());
    let capture = Capture::default();
    let report = run(&server, &snapshot, Some(&store), &capture)
        .await
        .unwrap();
    assert_eq!((report.items, report.attachments_missing), (1, 1));
    assert!(report.warned());
    let lines = capture.0.lock().unwrap();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("warn:") && line.contains("1 attachment")),
        "{lines:?}"
    );
}
