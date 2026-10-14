//! The encrypted backup directory.
//!
//! ```text
//! <backups>/key.age                       backup key, protected by the passphrase
//! <backups>/<role>/snapshots/vault-<time>.json.age
//! <backups>/<role>/snapshots/vault-<time>.meta.json
//! <backups>/<role>/attachments/<attachment id>.age
//! ```
//!
//! Everything is encrypted with [age](https://age-encryption.org), so a backup
//! can be read with the stock `age` tool and no part of this program.
//! Snapshots are small JSON documents. Attachment files are stored once each
//! and shared by every snapshot that references them.

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use age::secrecy::{ExposeSecret, SecretString};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime, PrimitiveDateTime, macros::format_description};

const KEY_FILE: &str = "key.age";
const SNAPSHOT_SUFFIX: &str = ".json.age";
const META_SUFFIX: &str = ".meta.json";

/// How backups are encrypted.
#[derive(Clone)]
pub enum Protection {
    /// A key generated on first use and stored in the backup directory,
    /// itself encrypted with this passphrase.
    Passphrase(String),
    /// age public keys. The machine writing backups cannot read them back.
    Recipients(Vec<String>),
}

/// The keys in use for one run.
pub struct Keys {
    recipients: Vec<age::x25519::Recipient>,
    /// Present in passphrase mode, or when an identity was supplied for a restore.
    identity: Option<age::x25519::Identity>,
}

impl Keys {
    pub fn open(backups: &Path, protection: &Protection) -> Result<Self> {
        match protection {
            Protection::Recipients(list) => {
                if list.is_empty() {
                    bail!("no backup recipients configured");
                }
                let recipients = list
                    .iter()
                    .map(|text| {
                        age::x25519::Recipient::from_str(text.trim())
                            .map_err(|error| anyhow!("invalid age recipient {text:?}: {error}"))
                    })
                    .collect::<Result<_>>()?;
                Ok(Self {
                    recipients,
                    identity: None,
                })
            }
            Protection::Passphrase(passphrase) => {
                let path = backups.join(KEY_FILE);
                let identity = if path.exists() {
                    read_key(&path, passphrase)?
                } else {
                    fs::create_dir_all(backups)
                        .with_context(|| format!("creating {}", backups.display()))?;
                    create_key(&path, passphrase)?
                };
                Ok(Self {
                    recipients: vec![identity.to_public()],
                    identity: Some(identity),
                })
            }
        }
    }

    /// Adds the identity needed to read backups made for recipients.
    pub fn with_identity_file(mut self, path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        self.identity = Some(parse_identity(&text)?);
        Ok(self)
    }

    fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        let encryptor = age::Encryptor::with_recipients(
            self.recipients.iter().map(|r| r as &dyn age::Recipient),
        )
        .map_err(|error| anyhow!("preparing backup encryption: {error}"))?;
        let mut out = Vec::with_capacity(data.len() + 512);
        let mut writer = encryptor.wrap_output(&mut out)?;
        writer.write_all(data)?;
        writer.finish()?;
        Ok(out)
    }

    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| anyhow!("reading a backup needs the age identity; pass --identity"))?;
        let decryptor = age::Decryptor::new(data)
            .map_err(|error| anyhow!("not a readable backup file: {error}"))?;
        let mut reader = decryptor
            .decrypt(std::iter::once(identity as &dyn age::Identity))
            .map_err(|error| anyhow!("the backup key does not open this file: {error}"))?;
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .context("the backup file is corrupt")?;
        Ok(out)
    }
}

fn read_key(path: &Path, passphrase: &str) -> Result<age::x25519::Identity> {
    let armored = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = decrypt_with_passphrase(&armored, passphrase).with_context(|| {
        format!(
            "the backup passphrase does not open {}; it was created with a different one",
            path.display()
        )
    })?;
    parse_identity(&text)
}

fn create_key(path: &Path, passphrase: &str) -> Result<age::x25519::Identity> {
    let identity = age::x25519::Identity::generate();
    let text = format!(
        "# vault-mirror backup key\n# public key: {}\n{}\n",
        identity.to_public(),
        identity.to_string().expose_secret()
    );
    let encrypted = encrypt_with_passphrase(text.as_bytes(), passphrase)?;
    match write_private_new(path, &encrypted) {
        Ok(()) => Ok(identity),
        // Another process created the key in the meantime. Replacing it would
        // leave whatever that process encrypted to it unreadable, so its key
        // is used instead.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_key(path, passphrase)
        }
        Err(error) => Err(error).with_context(|| format!("writing {}", path.display())),
    }
}

fn parse_identity(text: &str) -> Result<age::x25519::Identity> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("AGE-SECRET-KEY-"))
        .ok_or_else(|| anyhow!("no age secret key found"))?;
    age::x25519::Identity::from_str(line)
        .map_err(|error| anyhow!("invalid age secret key: {error}"))
}

/// scrypt cost for the key file: 2^16, which is 64 MiB of memory per attempt.
/// Fixed, because the library's default calibrates to the machine it runs on,
/// and a key written on a fast machine then fails to open on a slow or busy one.
const KEY_WORK_FACTOR: u8 = 16;
/// The highest cost accepted when opening a key file, as the `age` tool does.
const KEY_MAX_WORK_FACTOR: u8 = 22;

fn encrypt_with_passphrase(data: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let mut recipient = age::scrypt::Recipient::new(SecretString::from(passphrase.to_string()));
    recipient.set_work_factor(KEY_WORK_FACTOR);
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|error| anyhow!("preparing key encryption: {error}"))?;
    // Armored, so that `age -d -i key.age` accepts it as an identity file.
    let mut writer = encryptor.wrap_output(age::armor::ArmoredWriter::wrap_output(
        Vec::new(),
        age::armor::Format::AsciiArmor,
    )?)?;
    writer.write_all(data)?;
    Ok(writer.finish()?.finish()?)
}

fn decrypt_with_passphrase(data: &[u8], passphrase: &str) -> Result<String> {
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(data))
        .map_err(|error| anyhow!("not an age file: {error}"))?;
    let mut identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_string()));
    identity.set_max_work_factor(KEY_MAX_WORK_FACTOR);
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|error| anyhow!("{error}"))?;
    let mut out = String::new();
    reader.read_to_string(&mut out)?;
    Ok(out)
}

/// Writes a file that only the owner may read, atomically.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(directory)
        .with_context(|| format!("creating a file in {}", directory.display()))?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Like [`write_private`], but fails with `AlreadyExists` instead of
/// replacing a file that is already there.
fn write_private_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::other(format!("{} has no parent directory", path.display()))
    })?;
    let mut temp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(directory)?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best effort: a bind mount may not allow changing the mode.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// What is stored in the clear next to a snapshot. It holds nothing about the
/// vault's content: a salted fingerprint to recognise an unchanged vault, and
/// the ids of the attachment files the snapshot needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub format: u64,
    pub created_at: String,
    pub items: usize,
    pub salt: String,
    pub fingerprint: String,
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    pub created: OffsetDateTime,
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PruneReport {
    pub snapshots_removed: usize,
    pub attachments_removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention {
    pub days: u32,
    /// The newest snapshots that are kept whatever their age. At least one,
    /// because the newest snapshot of an unchanging vault only grows older.
    pub keep_at_least: usize,
}

/// The backups of one vault.
pub struct Store<'a> {
    keys: &'a Keys,
    snapshots: PathBuf,
    attachments: PathBuf,
}

impl<'a> Store<'a> {
    pub fn open(backups: &Path, role: &str, keys: &'a Keys) -> Result<Self> {
        let store = Self {
            keys,
            snapshots: backups.join(role).join("snapshots"),
            attachments: backups.join(role).join("attachments"),
        };
        create_private_dir(backups)?;
        create_private_dir(&backups.join(role))?;
        create_private_dir(&store.snapshots)?;
        create_private_dir(&store.attachments)?;
        Ok(store)
    }

    fn attachment_path(&self, attachment_id: &str) -> Result<PathBuf> {
        // Ids come from a server; never let one name a path.
        let safe = !attachment_id.is_empty()
            && attachment_id.len() <= 128
            && attachment_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !safe {
            bail!("refusing to use attachment id {attachment_id:?} as a file name");
        }
        Ok(self.attachments.join(format!("{attachment_id}.age")))
    }

    pub fn has_attachment(&self, attachment_id: &str) -> bool {
        self.attachment_path(attachment_id)
            .is_ok_and(|path| path.exists())
    }

    pub fn write_attachment(&self, attachment_id: &str, data: &[u8]) -> Result<()> {
        write_private(
            &self.attachment_path(attachment_id)?,
            &self.keys.encrypt(data)?,
        )
    }

    pub fn read_attachment(&self, attachment_id: &str) -> Result<Vec<u8>> {
        let path = self.attachment_path(attachment_id)?;
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        self.keys
            .decrypt(&data)
            .with_context(|| format!("decrypting {}", path.display()))
    }

    /// Snapshots, oldest first.
    pub fn list(&self) -> Result<Vec<SnapshotInfo>> {
        let mut snapshots = Vec::new();
        for entry in fs::read_dir(&self.snapshots)
            .with_context(|| format!("listing {}", self.snapshots.display()))?
        {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(SNAPSHOT_SUFFIX) else {
                continue;
            };
            let Some(created) = parse_snapshot_time(stem) else {
                continue;
            };
            let meta = fs::read(self.snapshots.join(format!("{stem}{META_SUFFIX}")))
                .ok()
                .and_then(|data| serde_json::from_slice(&data).ok());
            snapshots.push(SnapshotInfo {
                path,
                created,
                meta,
            });
        }
        // By time, then by the counter that separates snapshots of one second.
        snapshots.sort_by_key(|snapshot| {
            let name = snapshot
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (snapshot.created, name.len(), name)
        });
        Ok(snapshots)
    }

    /// Writes `document` as a new snapshot unless the newest snapshot already
    /// holds the same vault. `stamp` adds the creation time to the document.
    pub fn write_snapshot(
        &self,
        mut document: Value,
        attachment_ids: Vec<String>,
        now: OffsetDateTime,
        stamp: impl FnOnce(&mut Value, &str),
    ) -> Result<Option<PathBuf>> {
        let content = document.to_string();
        let items = document
            .get("items")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if let Some(Meta {
            salt, fingerprint, ..
        }) = self.list()?.last().and_then(|latest| latest.meta.as_ref())
            && fingerprint_of(salt, &content) == *fingerprint
        {
            return Ok(None);
        }

        let created_at = now
            .format(&time::format_description::well_known::Rfc3339)
            .context("formatting the snapshot time")?;
        stamp(&mut document, &created_at);
        let mut stem = format!(
            "vault-{}",
            now.format(format_description!(
                "[year][month][day]T[hour][minute][second]Z"
            ))
            .context("formatting the snapshot name")?
        );
        // Two snapshots within one second still need distinct, ordered names.
        let base = stem.clone();
        let mut suffix = 1;
        while self
            .snapshots
            .join(format!("{stem}{SNAPSHOT_SUFFIX}"))
            .exists()
        {
            stem = format!("{base}-{suffix}");
            suffix += 1;
        }

        let salt = uuid::Uuid::new_v4().simple().to_string();
        let meta = Meta {
            format: 1,
            created_at,
            items,
            fingerprint: fingerprint_of(&salt, &content),
            salt,
            attachments: attachment_ids,
        };
        let path = self.snapshots.join(format!("{stem}{SNAPSHOT_SUFFIX}"));
        let pretty = serde_json::to_vec_pretty(&document)?;
        write_private(&path, &self.keys.encrypt(&pretty)?)?;
        // Written second: a snapshot without metadata only costs a redundant
        // snapshot next time, while metadata without a snapshot would lie.
        write_private(
            &self.snapshots.join(format!("{stem}{META_SUFFIX}")),
            &serde_json::to_vec_pretty(&meta)?,
        )?;
        Ok(Some(path))
    }

    pub fn read_snapshot(&self, path: &Path) -> Result<Value> {
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let plain = self
            .keys
            .decrypt(&data)
            .with_context(|| format!("decrypting {}", path.display()))?;
        serde_json::from_slice(&plain)
            .with_context(|| format!("{} does not hold JSON", path.display()))
    }

    /// Removes snapshots past their retention, then attachment files that no
    /// remaining snapshot refers to.
    pub fn prune(&self, retention: &Retention, now: OffsetDateTime) -> Result<PruneReport> {
        let mut report = PruneReport::default();
        let snapshots = self.list()?;
        let cutoff = now - Duration::days(i64::from(retention.days));
        let keep_from = snapshots
            .len()
            .saturating_sub(retention.keep_at_least.max(1));
        let mut kept = Vec::new();
        for (index, snapshot) in snapshots.into_iter().enumerate() {
            if index >= keep_from || snapshot.created >= cutoff {
                kept.push(snapshot);
                continue;
            }
            fs::remove_file(&snapshot.path)
                .with_context(|| format!("removing {}", snapshot.path.display()))?;
            let _ = fs::remove_file(meta_path(&snapshot.path));
            report.snapshots_removed += 1;
        }

        // Without metadata for every kept snapshot it is unknown which files
        // are still needed, so nothing is collected.
        if kept.iter().any(|snapshot| snapshot.meta.is_none()) {
            return Ok(report);
        }
        let referenced: BTreeSet<&str> = kept
            .iter()
            .flat_map(|snapshot| {
                snapshot
                    .meta
                    .iter()
                    .flat_map(|meta| meta.attachments.iter().map(String::as_str))
            })
            .collect();
        for entry in fs::read_dir(&self.attachments)? {
            let path = entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".age"))
            else {
                continue;
            };
            if !referenced.contains(id) {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                report.attachments_removed += 1;
            }
        }
        Ok(report)
    }
}

fn meta_path(snapshot: &Path) -> PathBuf {
    let name = snapshot
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let stem = name.strip_suffix(SNAPSHOT_SUFFIX).unwrap_or(name);
    snapshot.with_file_name(format!("{stem}{META_SUFFIX}"))
}

fn fingerprint_of(salt: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

fn parse_snapshot_time(stem: &str) -> Option<OffsetDateTime> {
    let stamp = stem.strip_prefix("vault-")?.get(..16)?;
    let format = format_description!("[year][month][day]T[hour][minute][second]Z");
    PrimitiveDateTime::parse(stamp, format)
        .ok()
        .map(PrimitiveDateTime::assume_utc)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    fn passphrase(text: &str) -> Protection {
        Protection::Passphrase(text.to_string())
    }

    fn document(password: &str) -> Value {
        json!({"encrypted": false, "items": [{"id": "i", "login": {"password": password}}], "vaultMirror": {"format": 1}})
    }

    fn stamp(document: &mut Value, created_at: &str) {
        document["vaultMirror"]["createdAt"] = json!(created_at);
    }

    #[test]
    fn a_snapshot_round_trips_and_is_not_stored_in_the_clear() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("backup passphrase")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 02:30:00 UTC);

        let path = store
            .write_snapshot(document("hunter2"), vec![], now, stamp)
            .unwrap()
            .unwrap();
        assert_eq!(path.file_name().unwrap(), "vault-20261014T023000Z.json.age");
        for entry in walk(dir.path()) {
            let raw = fs::read(&entry).unwrap();
            assert!(
                !contains(&raw, b"hunter2"),
                "{} holds a secret in the clear",
                entry.display()
            );
        }
        let read = store.read_snapshot(&path).unwrap();
        assert_eq!(read["items"][0]["login"]["password"], "hunter2");
        assert_eq!(read["vaultMirror"]["createdAt"], "2026-10-14T02:30:00Z");

        // A later run with the same passphrase reads what an earlier run wrote.
        let reopened = Keys::open(dir.path(), &passphrase("backup passphrase")).unwrap();
        let store = Store::open(dir.path(), "source", &reopened).unwrap();
        assert_eq!(store.read_snapshot(&path).unwrap(), read);
    }

    #[test]
    fn a_different_passphrase_is_rejected_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        Keys::open(dir.path(), &passphrase("first")).unwrap();
        let error = Keys::open(dir.path(), &passphrase("second")).err().unwrap();
        assert!(format!("{error:#}").contains("different one"), "{error:#}");
    }

    #[test]
    fn runs_creating_the_key_at_the_same_time_end_up_with_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let start = std::sync::Barrier::new(2);
        let open = || {
            start.wait();
            Keys::open(dir.path(), &passphrase("p")).unwrap()
        };
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(open);
            let second = scope.spawn(open);
            (first.join().unwrap(), second.join().unwrap())
        });
        Store::open(dir.path(), "first", &first)
            .unwrap()
            .write_attachment("a", b"first")
            .unwrap();
        Store::open(dir.path(), "second", &second)
            .unwrap()
            .write_attachment("a", b"second")
            .unwrap();

        let kept = Keys::open(dir.path(), &passphrase("p")).unwrap();
        for role in ["first", "second"] {
            let store = Store::open(dir.path(), role, &kept).unwrap();
            assert_eq!(store.read_attachment("a").unwrap(), role.as_bytes());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key = fs::metadata(dir.path().join(KEY_FILE)).unwrap();
            assert_eq!(key.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn an_unchanged_vault_gets_no_second_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let t0 = datetime!(2026-10-14 02:30:00 UTC);

        assert!(
            store
                .write_snapshot(document("a"), vec![], t0, stamp)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .write_snapshot(document("a"), vec![], t0 + Duration::hours(1), stamp)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .write_snapshot(document("b"), vec![], t0 + Duration::hours(2), stamp)
                .unwrap()
                .is_some()
        );
        // Changing back is a change relative to the newest snapshot.
        assert!(
            store
                .write_snapshot(document("a"), vec![], t0 + Duration::hours(3), stamp)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.list().unwrap().len(), 3);
    }

    #[test]
    fn snapshots_in_the_same_second_do_not_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 02:30:00 UTC);
        let first = store
            .write_snapshot(document("a"), vec![], now, stamp)
            .unwrap()
            .unwrap();
        let second = store
            .write_snapshot(document("b"), vec![], now, stamp)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
        let listed: Vec<PathBuf> = store.list().unwrap().into_iter().map(|s| s.path).collect();
        assert_eq!(listed, [first, second]);
    }

    #[test]
    fn attachments_round_trip_and_reject_unsafe_ids() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        assert!(!store.has_attachment("abc123"));
        store.write_attachment("abc123", b"file content").unwrap();
        assert!(store.has_attachment("abc123"));
        assert_eq!(store.read_attachment("abc123").unwrap(), b"file content");
        for unsafe_id in ["../escape", "a/b", "", "a.b"] {
            assert!(
                store.write_attachment(unsafe_id, b"x").is_err(),
                "{unsafe_id}"
            );
            assert!(!store.has_attachment(unsafe_id));
        }
    }

    #[test]
    fn pruning_keeps_recent_snapshots_the_newest_ones_and_their_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 00:00:00 UTC);
        for (age_days, attachment) in [
            (90, "old-only"),
            (60, "shared"),
            (10, "shared"),
            (1, "recent"),
        ] {
            store.write_attachment(attachment, b"x").unwrap();
            let at = now - Duration::days(age_days);
            store
                .write_snapshot(
                    document(&age_days.to_string()),
                    vec![attachment.to_string()],
                    at,
                    stamp,
                )
                .unwrap();
        }
        store.write_attachment("orphan", b"x").unwrap();

        let retention = Retention {
            days: 30,
            keep_at_least: 1,
        };
        let report = store.prune(&retention, now).unwrap();
        assert_eq!(
            report,
            PruneReport {
                snapshots_removed: 2,
                attachments_removed: 2
            }
        );
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.has_attachment("shared") && store.has_attachment("recent"));
        assert!(!store.has_attachment("old-only") && !store.has_attachment("orphan"));

        // Far in the future everything is old, and the newest still stays.
        let later = now + Duration::days(365);
        let report = store
            .prune(
                &Retention {
                    days: 30,
                    keep_at_least: 0,
                },
                later,
            )
            .unwrap();
        assert_eq!(
            report,
            PruneReport {
                snapshots_removed: 1,
                attachments_removed: 1
            }
        );
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.has_attachment("recent"));
    }

    #[test]
    fn attachments_are_not_collected_when_a_snapshot_has_no_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 00:00:00 UTC);
        let path = store
            .write_snapshot(document("a"), vec!["needed".into()], now, stamp)
            .unwrap()
            .unwrap();
        store.write_attachment("needed", b"x").unwrap();
        store.write_attachment("unknown", b"x").unwrap();
        fs::remove_file(meta_path(&path)).unwrap();

        let report = store
            .prune(
                &Retention {
                    days: 30,
                    keep_at_least: 1,
                },
                now,
            )
            .unwrap();
        assert_eq!(report, PruneReport::default());
        assert!(store.has_attachment("unknown"));
    }

    #[test]
    fn recipient_mode_writes_backups_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let identity = age::x25519::Identity::generate();
        let protection = Protection::Recipients(vec![identity.to_public().to_string()]);
        let keys = Keys::open(dir.path(), &protection).unwrap();
        assert!(!dir.path().join(KEY_FILE).exists());
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let path = store
            .write_snapshot(
                document("a"),
                vec![],
                datetime!(2026-10-14 00:00:00 UTC),
                stamp,
            )
            .unwrap()
            .unwrap();
        assert!(format!("{:#}", store.read_snapshot(&path).unwrap_err()).contains("--identity"));

        let identity_file = dir.path().join("identity.txt");
        fs::write(
            &identity_file,
            format!("# comment\n{}\n", identity.to_string().expose_secret()),
        )
        .unwrap();
        let keys = Keys::open(dir.path(), &protection)
            .unwrap()
            .with_identity_file(&identity_file)
            .unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        assert_eq!(
            store.read_snapshot(&path).unwrap()["items"][0]["login"]["password"],
            "a"
        );

        assert!(
            Keys::open(
                dir.path(),
                &Protection::Recipients(vec!["not-a-key".into()])
            )
            .is_err()
        );
        assert!(Keys::open(dir.path(), &Protection::Recipients(vec![])).is_err());
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(walk(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
