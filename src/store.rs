//! The encrypted backup directory.
//!
//! ```text
//! <backups>/key.age                                      backup key, protected by the passphrase
//! <backups>/<role>/<id>/vault.json.age                   a snapshot
//! <backups>/<role>/<id>/attachments/<attachment id>.age  its attachment files
//! ```
//!
//! A snapshot's id is the UTC time it was made, such as `20261014T030000Z`,
//! with a counter for a second snapshot in the same second. A snapshot is
//! written into a temporary directory and renamed into place, so one that is
//! there is complete. Everything is encrypted with [age](https://age-encryption.org),
//! so a backup can be read with the stock `age` tool and no part of this program.

use std::{
    collections::HashMap,
    fs, io,
    io::{Read, Write},
    iter,
    path::{Path, PathBuf},
    str::FromStr,
};

use age::secrecy::{ExposeSecret, SecretString};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use time::{
    Duration, OffsetDateTime, PrimitiveDateTime, UtcOffset, format_description::BorrowedFormatItem,
    macros::format_description,
};

use crate::snapshot::{self, is_path_component};

const KEY_FILE: &str = "key.age";
const DOCUMENT_FILE: &str = "vault.json.age";
const ATTACHMENTS_DIR: &str = "attachments";
const TEMP_PREFIX: &str = ".tmp-";
const ROLES: [&str; 2] = ["source", "dest"];
/// The time in a snapshot's id, in UTC.
const ID_TIME: &[BorrowedFormatItem<'_>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");

/// The backup key, generated on first use and stored in the backup directory,
/// itself encrypted with the passphrase.
pub struct Keys {
    identity: age::x25519::Identity,
}

impl Keys {
    /// The key for writing backups. It is created on first use, but never in
    /// a directory that already holds snapshots: they need the old key, and a
    /// new one would only hide that it is gone.
    pub fn open(backups: &Path, passphrase: &str) -> Result<Self> {
        let path = backups.join(KEY_FILE);
        if path
            .try_exists()
            .with_context(|| format!("reading {}", path.display()))?
        {
            return Self::from_key_file(backups, passphrase);
        }
        for role in ROLES {
            if !list(backups, role)?.is_empty() {
                bail!(
                    "{} is missing, but {} holds backups that need it. Put the key file back \
                     from your copy of it, or set BACKUPS_DIR to an empty directory to start \
                     new backups",
                    path.display(),
                    backups.display()
                );
            }
        }
        create_private_dir(backups)?;
        let identity = age::x25519::Identity::generate();
        let text = format!(
            "# vault-mirror backup key\n# public key: {}\n{}\n",
            identity.to_public(),
            identity.to_string().expose_secret()
        );
        write_private(
            &path,
            &encrypt_with_passphrase(text.as_bytes(), passphrase)?,
        )?;
        Ok(Self { identity })
    }

    /// The key in `backups`, for reading. Creates nothing.
    pub fn from_key_file(backups: &Path, passphrase: &str) -> Result<Self> {
        let path = backups.join(KEY_FILE);
        let armored = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let text = decrypt_with_passphrase(&armored, passphrase).with_context(|| {
            format!(
                "the backup passphrase does not open {}; it was created with a different one",
                path.display()
            )
        })?;
        let line = (text.lines().map(str::trim))
            .find(|line| line.starts_with("AGE-SECRET-KEY-"))
            .ok_or_else(|| anyhow!("{} holds no age secret key", path.display()))?;
        let identity = age::x25519::Identity::from_str(line)
            .map_err(|error| anyhow!("{} holds an invalid key: {error}", path.display()))?;
        Ok(Self { identity })
    }

    fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        let recipient = self.identity.to_public();
        let encryptor =
            age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
                .map_err(|error| anyhow!("preparing backup encryption: {error}"))?;
        let mut out = Vec::with_capacity(data.len() + 512);
        let mut writer = encryptor.wrap_output(&mut out)?;
        writer.write_all(data)?;
        writer.finish()?;
        Ok(out)
    }

    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        let decryptor = age::Decryptor::new_buffered(data)
            .map_err(|error| anyhow!("not a readable backup file: {error}"))?;
        let mut reader = decryptor
            .decrypt(iter::once(&self.identity as &dyn age::Identity))
            .map_err(|error| anyhow!("the backup key does not open this file: {error}"))?;
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .context("the backup file is corrupt")?;
        Ok(out)
    }
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
    let encryptor = age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
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
        .decrypt(iter::once(&identity as &dyn age::Identity))
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
        .prefix(TEMP_PREFIX)
        .tempfile_in(directory)
        .with_context(|| format!("creating a file in {}", directory.display()))?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    sync_dir(directory)
}

pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best effort: a bind mount may not allow changing the mode.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Makes the names created or removed in `directory` durable, so that after
/// a crash no file written later is found without them.
fn sync_dir(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("syncing {}", directory.display()))?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

/// The file of an attachment in a snapshot's directory. Ids come from a server
/// and must never name a path.
fn attachment_path(snapshot: &Path, attachment_id: &str) -> Result<PathBuf> {
    if !is_path_component(attachment_id) {
        bail!("refusing to use {attachment_id:?} as a file name");
    }
    Ok(snapshot
        .join(ATTACHMENTS_DIR)
        .join(format!("{attachment_id}.age")))
}

/// The time and counter in a snapshot id, and `None` for any other name, so
/// that pruning never removes a directory vault-mirror didn't write.
fn parse_id(id: &str) -> Option<(OffsetDateTime, u64)> {
    let (stamp, counter) = id.split_at_checked(16)?;
    let counter = match counter.strip_prefix('-') {
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse().ok()?
        }
        None if counter.is_empty() => 0,
        _ => return None,
    };
    Some((
        PrimitiveDateTime::parse(stamp, ID_TIME).ok()?.assume_utc(),
        counter,
    ))
}

/// The snapshots in `directory`, oldest first, with when each was made.
fn scan(directory: &Path) -> Result<Vec<(OffsetDateTime, String)>> {
    let entries = match fs::read_dir(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries.with_context(|| format!("listing {}", directory.display()))?,
    };
    let mut found = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some((created, counter)) = name.to_str().and_then(parse_id) {
            found.push((created, counter, name.to_string_lossy().into_owned()));
        }
    }
    found.sort();
    Ok(found
        .into_iter()
        .map(|(created, _, id)| (created, id))
        .collect())
}

/// The ids of the snapshots of `role` in `backups`, oldest first. Needs no
/// key and creates nothing.
pub fn list(backups: &Path, role: &str) -> Result<Vec<String>> {
    Ok(scan(&backups.join(role))?
        .into_iter()
        .map(|(_, id)| id)
        .collect())
}

/// The backups of one vault.
pub struct Store<'a> {
    keys: &'a Keys,
    directory: PathBuf,
}

impl<'a> Store<'a> {
    /// The backups of `role`. Creates nothing until a snapshot is written, so
    /// a read-only copy of a backup directory works too.
    pub fn new(backups: &Path, role: &str, keys: &'a Keys) -> Self {
        Self {
            keys,
            directory: backups.join(role),
        }
    }

    /// Snapshot ids, oldest first.
    pub fn list(&self) -> Result<Vec<String>> {
        Ok(scan(&self.directory)?
            .into_iter()
            .map(|(_, id)| id)
            .collect())
    }

    /// The snapshot `id`, which need not exist.
    pub fn snapshot(&self, id: &str) -> Snapshot<'a> {
        Snapshot {
            keys: self.keys,
            directory: self.directory.join(id),
        }
    }

    /// Whether the newest snapshot holds `document` and every attachment file
    /// it names opens, so that a run needs no new one. Anything unreadable
    /// gives `false`: a needless snapshot costs only space.
    pub fn unchanged(&self, document: &Value) -> Result<bool> {
        let Some(id) = self.list()?.pop() else {
            return Ok(false);
        };
        let newest = self.snapshot(&id);
        Ok(newest.document().is_ok_and(|stored| stored == *document)
            && snapshot::parse(document)?
                .attachments()
                .all(|attachment| newest.attachment(&attachment.attachment_id).is_ok()))
    }

    /// Starts a snapshot made at `now`. Call with the backup directory
    /// locked: it removes what earlier runs left unfinished.
    pub fn begin(&self, now: OffsetDateTime) -> Result<SnapshotWriter<'_>> {
        create_private_dir(&self.directory)?;
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        let temp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempdir_in(&self.directory)
            .with_context(|| format!("creating a directory in {}", self.directory.display()))?;
        fs::create_dir(temp.path().join(ATTACHMENTS_DIR))?;
        Ok(SnapshotWriter {
            store: self,
            temp,
            now,
        })
    }

    /// Removes the snapshots past retention. Those from the last day all
    /// stay; older days keep only their newest, for `retention_days`. The
    /// newest snapshot always stays, since an unchanged vault only makes it
    /// older. Returns how many were removed.
    pub fn prune(&self, retention_days: u32, now: OffsetDateTime) -> Result<usize> {
        let snapshots = scan(&self.directory)?;
        let recent = now - Duration::days(1);
        let cutoff = now - Duration::days(i64::from(retention_days));
        let newest_of_day: HashMap<time::Date, usize> = (snapshots.iter().enumerate())
            .map(|(index, (created, _))| (created.date(), index))
            .collect();
        let mut removed = 0;
        for (index, (created, id)) in snapshots.iter().enumerate() {
            let keep = index + 1 == snapshots.len()
                || *created >= recent
                || (*created >= cutoff && newest_of_day[&created.date()] == index);
            if keep {
                continue;
            }
            // Renamed first, so that a crash part way leaves no snapshot that
            // lacks its files.
            let doomed = self.directory.join(format!("{TEMP_PREFIX}{id}"));
            fs::rename(self.directory.join(id), &doomed)
                .with_context(|| format!("removing snapshot {id}"))?;
            fs::remove_dir_all(&doomed).with_context(|| format!("removing snapshot {id}"))?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// One snapshot of a vault.
pub struct Snapshot<'a> {
    keys: &'a Keys,
    directory: PathBuf,
}

impl Snapshot<'_> {
    pub fn document(&self) -> Result<Value> {
        let path = self.directory.join(DOCUMENT_FILE);
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let plain = self
            .keys
            .decrypt(&data)
            .with_context(|| format!("decrypting {}", path.display()))?;
        serde_json::from_slice(&plain)
            .with_context(|| format!("{} does not hold JSON", path.display()))
    }

    pub fn attachment(&self, attachment_id: &str) -> Result<Vec<u8>> {
        let path = attachment_path(&self.directory, attachment_id)?;
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        self.keys
            .decrypt(&data)
            .with_context(|| format!("decrypting {}", path.display()))
    }
}

/// A snapshot being written. Dropped before [`SnapshotWriter::finish`], it
/// removes what it wrote.
pub struct SnapshotWriter<'s> {
    store: &'s Store<'s>,
    temp: tempfile::TempDir,
    now: OffsetDateTime,
}

impl SnapshotWriter<'_> {
    pub fn write_attachment(&self, attachment_id: &str, data: &[u8]) -> Result<()> {
        let path = attachment_path(self.temp.path(), attachment_id)?;
        write_private(&path, &self.store.keys.encrypt(data)?)
    }

    /// Writes `document` and moves the snapshot into place. Returns its id.
    pub fn finish(self, document: &Value) -> Result<String> {
        let data = self
            .store
            .keys
            .encrypt(&serde_json::to_vec_pretty(document)?)?;
        write_private(&self.temp.path().join(DOCUMENT_FILE), &data)?;
        sync_dir(self.temp.path())?;
        let base = (self.now.to_offset(UtcOffset::UTC))
            .format(ID_TIME)
            .context("formatting the snapshot id")?;
        let mut id = base.clone();
        for counter in 1.. {
            if !self.store.directory.join(&id).try_exists()? {
                break;
            }
            id = format!("{base}-{counter}");
        }
        let temp = self.temp.keep();
        let path = self.store.directory.join(&id);
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_dir_all(&temp);
            return Err(error).with_context(|| format!("writing {}", path.display()));
        }
        sync_dir(&self.store.directory)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    fn document(notes: &str, attachments: &[&str]) -> Value {
        let attachments: Vec<Value> = (attachments.iter())
            .map(|id| json!({"itemId": "item-1", "attachmentId": id, "fileName": "a.txt"}))
            .collect();
        json!({
            "encrypted": false,
            "folders": [],
            "items": [{"id": "item-1", "type": 2, "name": "Note", "notes": notes, "secureNote": {"type": 0}}],
            "vaultMirror": {"scope": "personal", "plainStrings": {}, "attachments": attachments},
        })
    }

    fn write(
        store: &Store<'_>,
        now: OffsetDateTime,
        notes: &str,
        files: &[(&str, &[u8])],
    ) -> String {
        let writer = store.begin(now).unwrap();
        for (id, data) in files {
            writer.write_attachment(id, data).unwrap();
        }
        let ids: Vec<&str> = files.iter().map(|(id, _)| *id).collect();
        writer.finish(&document(notes, &ids)).unwrap()
    }

    fn files_under(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            match path.is_dir() {
                true => found.extend(files_under(&path)),
                false => found.push(path),
            }
        }
        found
    }

    #[test]
    fn a_snapshot_round_trips_and_is_not_stored_in_the_clear() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), "p").unwrap();
        let store = Store::new(dir.path(), "source", &keys);
        let id = write(
            &store,
            datetime!(2026-10-14 03:00 UTC),
            "secret note",
            &[("att-1", b"secret file")],
        );
        assert_eq!(id, "20261014T030000Z");
        assert_eq!(list(dir.path(), "source").unwrap(), [id.as_str()]);

        let keys = Keys::from_key_file(dir.path(), "p").unwrap();
        let snapshot = Store::new(dir.path(), "source", &keys).snapshot(&id);
        assert_eq!(
            snapshot.document().unwrap()["items"][0]["notes"],
            "secret note"
        );
        assert_eq!(snapshot.attachment("att-1").unwrap(), b"secret file");
        assert!(snapshot.attachment("../key").is_err());

        for path in files_under(dir.path()) {
            let data = fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&data);
            assert!(!text.contains("secret"), "{}", path.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o077, 0, "{}", path.display());
            }
        }
    }

    #[test]
    fn a_different_passphrase_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        Keys::open(dir.path(), "p").unwrap();
        let error = Keys::open(dir.path(), "q").err().unwrap();
        assert!(format!("{error:#}").contains("different one"), "{error:#}");
    }

    #[test]
    fn a_missing_key_is_not_replaced_in_a_directory_that_holds_backups() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), "p").unwrap();
        write(
            &Store::new(dir.path(), "dest", &keys),
            datetime!(2026-10-14 00:00 UTC),
            "a",
            &[],
        );
        fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        let error = Keys::open(dir.path(), "p").err().unwrap();
        assert!(format!("{error:#}").contains("is missing"), "{error:#}");
        assert!(!dir.path().join(KEY_FILE).exists());
    }

    #[test]
    fn reading_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("backups");
        assert!(Keys::from_key_file(&backups, "p").is_err());
        assert_eq!(list(&backups, "source").unwrap(), Vec::<String>::new());
        assert!(!backups.exists());
    }

    #[test]
    fn a_writer_that_stops_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), "p").unwrap();
        let store = Store::new(dir.path(), "source", &keys);
        let writer = store.begin(datetime!(2026-10-14 00:00 UTC)).unwrap();
        writer.write_attachment("att-1", b"x").unwrap();
        assert!(writer.write_attachment("../att", b"x").is_err());
        drop(writer);
        assert_eq!(store.list().unwrap(), Vec::<String>::new());
        assert_eq!(fs::read_dir(dir.path().join("source")).unwrap().count(), 0);

        // What a crashed run left is removed by the next.
        let crashed = store.begin(datetime!(2026-10-14 00:00 UTC)).unwrap();
        let left = crashed.temp.keep();
        write(&store, datetime!(2026-10-14 01:00 UTC), "a", &[]);
        assert!(!left.exists());
    }

    #[test]
    fn snapshots_in_the_same_second_get_their_own_ids_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), "p").unwrap();
        let store = Store::new(dir.path(), "source", &keys);
        let at = datetime!(2026-10-14 02:30 +01:00);
        let ids: Vec<String> = (0..11)
            .map(|n| write(&store, at, &n.to_string(), &[]))
            .collect();
        assert_eq!(ids[0], "20261014T013000Z");
        assert_eq!(ids[10], "20261014T013000Z-10");
        assert_eq!(store.list().unwrap(), ids);
    }

    #[test]
    fn an_unchanged_vault_is_skipped_only_after_its_newest_snapshot_checks_out() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), "p").unwrap();
        let store = Store::new(dir.path(), "source", &keys);
        assert!(!store.unchanged(&document("a", &[])).unwrap());
        let id = write(
            &store,
            datetime!(2026-10-14 00:00 UTC),
            "a",
            &[("att-1", b"x")],
        );
        assert!(store.unchanged(&document("a", &["att-1"])).unwrap());
        assert!(!store.unchanged(&document("b", &["att-1"])).unwrap());
        assert!(!store.unchanged(&document("a", &[])).unwrap());

        // A damaged attachment file is no backup.
        let file = attachment_path(&dir.path().join("source").join(&id), "att-1").unwrap();
        fs::write(&file, b"damaged").unwrap();
        assert!(!store.unchanged(&document("a", &["att-1"])).unwrap());
    }

    #[test]
    fn pruning_keeps_the_last_day_one_a_day_within_retention_and_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), "p").unwrap();
        let store = Store::new(dir.path(), "source", &keys);
        let now = datetime!(2026-10-15 12:00 UTC);
        let made: Vec<String> = [24 * 40, 24 * 3 + 2, 24 * 3 + 1, 48, 23, 1]
            .into_iter()
            .map(|hours| write(&store, now - Duration::hours(hours), "a", &[]))
            .collect();
        // Gone: the one past retention, and the older one of its day.
        assert_eq!(store.prune(30, now).unwrap(), 2);
        assert_eq!(store.list().unwrap(), made[2..]);

        // The newest stays however old it is.
        assert_eq!(store.prune(30, now + Duration::days(400)).unwrap(), 3);
        assert_eq!(store.list().unwrap(), made[5..]);
    }
}
