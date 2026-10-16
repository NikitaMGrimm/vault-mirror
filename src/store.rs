//! The encrypted backup directory.
//!
//! ```text
//! <backups>/key.age                                     backup key, protected by the passphrase
//! <backups>/<role>/snapshots/<id>.json.age              a snapshot
//! <backups>/<role>/snapshots/<id>.meta.json             what it is, written last
//! <backups>/<role>/attachments/<id>/<attachment id>.age its attachment files
//! <backups>/<role>/attachments/<attachment id>.age      files of format 1 snapshots
//! ```
//!
//! A snapshot's id is `vault-<time>`, with a counter for a second snapshot in
//! the same second. Everything is encrypted with [age](https://age-encryption.org),
//! so a backup can be read with the stock `age` tool and no part of this program.
//!
//! Each snapshot has attachment files of its own and records their digests in
//! its encrypted document. So a file that is damaged, or encrypted to a key
//! since replaced, never stands in for a newer snapshot's, and no file changes
//! once written. Format 1 snapshots shared one directory of attachment files;
//! they stay readable and are never pruned.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Mutex, PoisonError},
};

use age::secrecy::{ExposeSecret, SecretString};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::{
    Duration, OffsetDateTime, PrimitiveDateTime, UtcOffset,
    format_description::{BorrowedFormatItem, well_known::Rfc3339},
    macros::format_description,
};

use crate::{
    snapshot::{self, AttachmentStorage, SnapshotAttachment},
    text::count,
};

const KEY_FILE: &str = "key.age";
const SNAPSHOT_SUFFIX: &str = ".json.age";
const META_SUFFIX: &str = ".meta.json";
const ATTACHMENT_SUFFIX: &str = ".age";
const TEMP_PREFIX: &str = ".tmp-";
/// The time in a snapshot's id, in UTC.
const ID_TIME: &[BorrowedFormatItem<'_>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");
const META_FORMAT: u64 = 2;
/// The bookkeeping block of a snapshot document, as `snapshot` names it.
const BLOCK: &str = "vaultMirror";
/// What a run writes in the backup directory before it opens the key, as
/// `app` names them.
const RUN_FILES: [&str; 3] = ["run.lock", "state.json", "last-run.json"];

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
    /// Whom backups are encrypted to. Empty in keys that only read backups.
    recipients: Vec<age::x25519::Recipient>,
    identities: Vec<age::x25519::Identity>,
}

impl Keys {
    /// The keys for writing backups. A passphrase's key is created on first
    /// use, but never in a directory that already holds backups: they need
    /// the old key, and a new one would only hide that it is gone.
    pub fn open(backups: &Path, protection: &Protection) -> Result<Self> {
        match protection {
            Protection::Recipients(list) => {
                if list.is_empty() {
                    bail!("no backup recipients configured");
                }
                // The text may be a private key pasted by mistake, and the
                // parser's error can quote it, so neither is repeated.
                let recipients = list
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        age::x25519::Recipient::from_str(text.trim()).map_err(|_| {
                            anyhow!(
                                "BACKUP_RECIPIENTS entry {} is not an age public key. Give \
                                 the public key, which starts with age1",
                                index + 1
                            )
                        })
                    })
                    .collect::<Result<_>>()?;
                Ok(Self {
                    recipients,
                    identities: Vec::new(),
                })
            }
            Protection::Passphrase(passphrase) => {
                let path = backups.join(KEY_FILE);
                let identities = if path
                    .try_exists()
                    .with_context(|| format!("reading {}", path.display()))?
                {
                    read_key(&path, passphrase)?
                } else if holds_backups(backups)? {
                    bail!(
                        "{} is missing, but {} holds backups that need it. Put the key file back \
                         from your copy of it, or set BACKUPS_DIR to an empty directory to start \
                         new backups",
                        path.display(),
                        backups.display()
                    );
                } else {
                    create_private_dir(backups)?;
                    create_key(&path, passphrase)?
                };
                Ok(Self {
                    recipients: identities.iter().map(|i| i.to_public()).collect(),
                    identities,
                })
            }
        }
    }

    /// Keys that read the backups in `backups` with the passphrase's key.
    /// Creates nothing.
    pub fn from_key_file(backups: &Path, passphrase: &str) -> Result<Self> {
        Ok(Self {
            recipients: Vec::new(),
            identities: read_key(&backups.join(KEY_FILE), passphrase)?,
        })
    }

    /// Keys that read backups with every age identity in `path`.
    pub fn from_identity_file(path: &Path) -> Result<Self> {
        Ok(Self {
            recipients: Vec::new(),
            identities: read_identities(path)?,
        })
    }

    /// Adds the identities in `path`, such as those of earlier recipients.
    pub fn with_identity_file(mut self, path: &Path) -> Result<Self> {
        self.identities.extend(read_identities(path)?);
        Ok(self)
    }

    /// Whether these keys open what they encrypt, so that a backup can be
    /// checked by reading it back.
    fn reads_own(&self) -> bool {
        self.identities
            .iter()
            .any(|identity| self.recipients.contains(&identity.to_public()))
    }

    fn recipient_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.recipients.iter().map(|r| r.to_string()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Names the recipients in the clear metadata, so that pruning keeps
    /// snapshots that other keys open.
    fn encryption_id(&self) -> String {
        let canonical = format!(
            "vault-mirror encryption 1\n{}",
            self.recipient_names().join("\n")
        );
        sha256_hex(canonical.as_bytes())
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
        let mut out = Vec::new();
        self.decrypt_to(data, &mut out)?;
        Ok(out)
    }

    fn decrypt_to(&self, data: impl BufRead, out: &mut impl Write) -> Result<()> {
        if self.identities.is_empty() {
            bail!("reading a backup needs the age identity; pass --identity");
        }
        let decryptor = age::Decryptor::new_buffered(data)
            .map_err(|error| anyhow!("not a readable backup file: {error}"))?;
        let mut reader = decryptor
            .decrypt(self.identities.iter().map(|i| i as &dyn age::Identity))
            .map_err(|error| anyhow!("the backup key does not open this file: {error}"))?;
        io::copy(&mut reader, out).context("the backup file is corrupt")?;
        Ok(())
    }
}

/// Whether `backups` holds anything but empty directories and the files a
/// run writes before it opens the key.
fn holds_backups(backups: &Path) -> Result<bool> {
    for path in paths_in(backups)? {
        let expected = if is_dir(&path)? {
            !holds_files(&path)?
        } else {
            let name = file_name(&path);
            RUN_FILES.contains(&name) || name.starts_with(TEMP_PREFIX)
        };
        if !expected {
            return Ok(true);
        }
    }
    Ok(false)
}

fn holds_files(directory: &Path) -> Result<bool> {
    for path in paths_in(directory)? {
        if !is_dir(&path)? || holds_files(&path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `path` is a directory itself, not a link to one.
fn is_dir(path: &Path) -> Result<bool> {
    Ok(fs::symlink_metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .is_dir())
}

/// What `directory` holds, or nothing when it doesn't exist.
fn paths_in(directory: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries.with_context(|| format!("listing {}", directory.display()))?,
    };
    entries
        .map(|entry| Ok(entry?.path()))
        .collect::<io::Result<_>>()
        .with_context(|| format!("listing {}", directory.display()))
}

/// The file name of `path`, or "" when it is not valid Unicode.
fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

/// The id of the snapshot file at `path`, if it is named like one.
fn snapshot_id_of(path: &Path) -> Option<&str> {
    file_name(path).strip_suffix(SNAPSHOT_SUFFIX)
}

fn read_key(path: &Path, passphrase: &str) -> Result<Vec<age::x25519::Identity>> {
    let armored = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = decrypt_with_passphrase(&armored, passphrase).with_context(|| {
        format!(
            "the backup passphrase does not open {}; it was created with a different one",
            path.display()
        )
    })?;
    parse_identities(&text)
}

fn create_key(path: &Path, passphrase: &str) -> Result<Vec<age::x25519::Identity>> {
    let identity = age::x25519::Identity::generate();
    let text = format!(
        "# vault-mirror backup key\n# public key: {}\n{}\n",
        identity.to_public(),
        identity.to_string().expose_secret()
    );
    let encrypted = encrypt_with_passphrase(text.as_bytes(), passphrase)?;
    match write_private_new(path, &encrypted) {
        Ok(()) => Ok(vec![identity]),
        // Another process created the key in the meantime. Replacing it would
        // leave whatever that process encrypted to it unreadable, so its key
        // is used instead.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_key(path, passphrase),
        Err(error) => Err(error).with_context(|| format!("writing {}", path.display())),
    }
}

fn read_identities(path: &Path) -> Result<Vec<age::x25519::Identity>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_identities(&text).with_context(|| format!("reading {}", path.display()))
}

/// Every age secret key in an identity file. Comments and other lines are
/// skipped.
fn parse_identities(text: &str) -> Result<Vec<age::x25519::Identity>> {
    let identities: Vec<_> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("AGE-SECRET-KEY-"))
        .map(|line| {
            age::x25519::Identity::from_str(line)
                .map_err(|error| anyhow!("invalid age secret key: {error}"))
        })
        .collect::<Result<_>>()?;
    if identities.is_empty() {
        bail!("no age secret key found");
    }
    Ok(identities)
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
        .prefix(TEMP_PREFIX)
        .tempfile_in(directory)
        .with_context(|| format!("creating a file in {}", directory.display()))?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    sync_dir(directory)?;
    Ok(())
}

/// Like [`write_private`], but fails with `AlreadyExists` instead of
/// replacing a file that is already there.
pub(crate) fn write_private_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no parent directory", path.display())))?;
    let mut temp = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(directory)?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|error| error.error)?;
    sync_dir(directory)?;
    Ok(())
}

pub(crate) fn create_private_dir(path: &Path) -> Result<()> {
    let created: Vec<&Path> = path
        .ancestors()
        .take_while(|directory| !directory.as_os_str().is_empty() && !directory.is_dir())
        .collect();
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    restrict(path);
    // Only new directory entries need publication. Existing ancestors may
    // permit traversal without granting the read access fsync needs.
    for directory in created {
        sync_dir(directory)?;
        let parent = directory
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        sync_dir(parent)?;
    }
    Ok(())
}

fn restrict(directory: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best effort: a bind mount may not allow changing the mode.
        let _ = fs::set_permissions(directory, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = directory;
}

/// Makes the names created or removed in `directory` durable, so that after
/// a crash no file written later is found without them.
pub(crate) fn sync_dir(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("syncing {}: {error}", directory.display()),
            )
        })?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Fails unless `data`, read from `path`, is the file a backup recorded.
fn ensure_digest(data: &[u8], expected: &str, path: &Path) -> Result<()> {
    if sha256_hex(data) != expected {
        bail!(
            "{} is not the file the backup recorded; it is damaged or was replaced",
            path.display()
        );
    }
    Ok(())
}

/// `text`, if it is safe as a file name. Checked here too, because ids come
/// from a server and must never name a path.
fn path_component(text: &str) -> Result<&str> {
    if !snapshot::is_path_component(text) {
        bail!("refusing to use {text:?} as a file name");
    }
    Ok(text)
}

fn attachment_file_name(attachment_id: &str) -> Result<String> {
    Ok(format!(
        "{}{ATTACHMENT_SUFFIX}",
        path_component(attachment_id)?
    ))
}

/// Which vault a snapshot holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultOrigin {
    /// The server's API address.
    pub api_url: String,
    /// The account that read the vault.
    pub account_id: String,
    /// The organization, or `None` for the account's individual vault.
    pub organization_id: Option<String>,
}

impl VaultOrigin {
    /// Names the vault in the clear metadata without spelling it out.
    fn id(&self) -> String {
        let canonical = format!(
            "vault-mirror origin 1\n{}\n{}\n{}",
            self.api_url,
            self.account_id,
            self.organization_id.as_deref().unwrap_or("")
        );
        sha256_hex(canonical.as_bytes())
    }
}

/// What is stored in the clear next to a snapshot. It holds nothing about
/// the vault's content: counts, the time, and opaque names for the vault and
/// the keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    /// 1 for a snapshot that shares attachment files with others.
    pub format: u64,
    pub created_at: String,
    pub items: usize,
    pub attachment_files: usize,
    /// The vault and keys of a format 2 snapshot, the only kind pruned.
    group: Option<Group>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    origin: String,
    encryption: String,
}

/// The metadata file of a format 2 snapshot.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaFile {
    format: u64,
    created_at: String,
    items: usize,
    attachment_files: usize,
    origin: String,
    encryption: String,
    /// Ties this metadata to one snapshot file.
    snapshot_sha256: String,
}

/// The metadata file of a format 1 snapshot. Its salt and fingerprint let
/// anyone with the file test a guess at the vault's content, so
/// [`migrate_metadata`] removes them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMetaFile {
    #[serde(rename = "format")]
    _format: u64,
    created_at: String,
    items: usize,
    attachments: Vec<String>,
    #[serde(default, rename = "vault")]
    _vault: Option<String>,
    #[serde(default, rename = "salt")]
    _salt: Option<String>,
    #[serde(default, rename = "fingerprint")]
    _fingerprint: Option<String>,
}

enum Sidecar {
    Missing,
    /// Unreadable, or not metadata this version understands.
    Unknown,
    Legacy(LegacyMetaFile),
    Current(MetaFile),
}

impl Sidecar {
    fn read(path: &Path) -> Self {
        match fs::read(path) {
            Ok(data) => Self::parse(&data),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::Missing,
            Err(_) => Self::Unknown,
        }
    }

    fn parse(data: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            return Self::Unknown;
        };
        match value.get("format").and_then(Value::as_u64) {
            Some(1) => serde_json::from_value(value).map_or(Self::Unknown, Self::Legacy),
            Some(META_FORMAT) => match serde_json::from_value::<MetaFile>(value) {
                Ok(meta)
                    if [&meta.origin, &meta.encryption, &meta.snapshot_sha256]
                        .iter()
                        .all(|digest| is_sha256_hex(digest)) =>
                {
                    Self::Current(meta)
                }
                _ => Self::Unknown,
            },
            _ => Self::Unknown,
        }
    }

    fn recognised(&self) -> bool {
        matches!(self, Self::Legacy(_) | Self::Current(_))
    }

    fn meta(&self) -> Option<Meta> {
        match self {
            Self::Missing | Self::Unknown => None,
            Self::Legacy(meta) => Some(Meta {
                format: 1,
                created_at: meta.created_at.clone(),
                items: meta.items,
                attachment_files: meta.attachments.len(),
                group: None,
            }),
            Self::Current(meta) => Some(Meta {
                format: meta.format,
                created_at: meta.created_at.clone(),
                items: meta.items,
                attachment_files: meta.attachment_files,
                group: Some(Group {
                    origin: meta.origin.clone(),
                    encryption: meta.encryption.clone(),
                }),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    pub created: OffsetDateTime,
    /// `None` when there is no metadata this version reads.
    pub meta: Option<Meta>,
}

struct Found {
    id: String,
    path: PathBuf,
    created: OffsetDateTime,
    counter: u64,
    sidecar: Sidecar,
    /// Whether it has an attachment directory, which a format 2 snapshot
    /// creates before anything else.
    reserved: bool,
}

impl Found {
    /// A format 2 snapshot whose writing stopped before its metadata, so it
    /// may lack files it promises.
    fn unfinished(&self) -> bool {
        self.reserved && !self.sidecar.recognised()
    }

    fn into_info(self) -> SnapshotInfo {
        SnapshotInfo {
            meta: self.sidecar.meta(),
            path: self.path,
            created: self.created,
        }
    }
}

/// Every snapshot file in `snapshots`, oldest first.
fn scan(snapshots: &Path, attachments: &Path) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for path in paths_in(snapshots)? {
        let Some(id) = snapshot_id_of(&path).map(str::to_string) else {
            continue;
        };
        let Some((created, counter)) = parse_snapshot_id(&id) else {
            continue;
        };
        found.push(Found {
            sidecar: Sidecar::read(&snapshots.join(format!("{id}{META_SUFFIX}"))),
            reserved: attachments.join(&id).is_dir(),
            id,
            path,
            created,
            counter,
        });
    }
    found.sort_by_key(|found| (found.created, found.counter));
    Ok(found)
}

fn listed(snapshots: &Path, attachments: &Path) -> Result<Vec<SnapshotInfo>> {
    Ok(scan(snapshots, attachments)?
        .into_iter()
        .filter(|found| !found.unfinished())
        .map(Found::into_info)
        .collect())
}

/// The snapshots of `role` in `backups`, oldest first. Needs no keys and
/// creates nothing. A snapshot whose writing never finished is left out, so
/// it is never taken for the newest; it can still be read by its path.
pub fn list(backups: &Path, role: &str) -> Result<Vec<SnapshotInfo>> {
    let role = backups.join(role);
    listed(&role.join("snapshots"), &role.join("attachments"))
}

/// Metadata files in `snapshots` that this version doesn't understand.
fn unreadable_metadata(snapshots: &Path) -> Result<Vec<PathBuf>> {
    let mut unreadable: Vec<PathBuf> = paths_in(snapshots)?
        .into_iter()
        .filter(|path| file_name(path).ends_with(META_SUFFIX))
        .filter(|path| !Sidecar::read(path).recognised())
        .collect();
    unreadable.sort();
    Ok(unreadable)
}

fn unreadable_error(paths: &[PathBuf], consequence: &str) -> anyhow::Error {
    let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    anyhow!(
        "this version of vault-mirror can't read the snapshot metadata in {}, so {consequence}. \
         Move those files out of the backup directory once you have checked them, or update \
         vault-mirror if a newer version wrote them",
        paths.join(", ")
    )
}

/// Removes the salt and fingerprint that the metadata of format 1 snapshots
/// held in the clear: together they let anyone with the file test a guess at
/// the vault's content. Each file is replaced atomically and keeps its other
/// fields. Call with the backup directory locked.
///
/// Metadata this version can't read is left as it is. It is named in the
/// error, after every other file is done.
pub fn migrate_metadata(backups: &Path) -> Result<()> {
    let mut unreadable = Vec::new();
    for role in paths_in(backups)? {
        let snapshots = role.join("snapshots");
        if !snapshots.is_dir() {
            continue;
        }
        for path in paths_in(&snapshots)? {
            let name = file_name(&path);
            let sidecar = name.ends_with(META_SUFFIX);
            // A run killed while writing metadata leaves its temporary file,
            // which can hold a fingerprint too. Other temporary files are
            // encrypted.
            if !sidecar && !name.starts_with(TEMP_PREFIX) {
                continue;
            }
            let recognised = scrub(&path)?;
            if sidecar && !recognised {
                unreadable.push(path);
            }
        }
    }
    if unreadable.is_empty() {
        return Ok(());
    }
    unreadable.sort();
    Err(unreadable_error(
        &unreadable,
        "it can't remove the content fingerprints that older versions stored there",
    ))
}

/// Removes the salt and fingerprint from one metadata file. `false` when the
/// file is not metadata this version reads.
fn scrub(path: &Path) -> Result<bool> {
    let Ok(data) = fs::read(path) else {
        return Ok(false);
    };
    if !Sidecar::parse(&data).recognised() {
        return Ok(false);
    }
    let Ok(Value::Object(mut fields)) = serde_json::from_slice(&data) else {
        return Ok(false);
    };
    let salt = fields.shift_remove("salt");
    let fingerprint = fields.shift_remove("fingerprint");
    if salt.is_some() || fingerprint.is_some() {
        write_private(path, &serde_json::to_vec_pretty(&fields)?)?;
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PruneReport {
    pub snapshots_removed: usize,
    pub attachments_removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention {
    /// Snapshots older than this many days are removed. Below that, a day
    /// keeps only its newest snapshot once it is more than a day old.
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
    /// The backups of `role`, for reading. Creates nothing, so a read-only
    /// copy of a backup directory works too.
    pub fn new(backups: &Path, role: &str, keys: &'a Keys) -> Self {
        Self {
            keys,
            snapshots: backups.join(role).join("snapshots"),
            attachments: backups.join(role).join("attachments"),
        }
    }

    /// The backups of `role`, for writing. Creates their directories.
    pub fn open(backups: &Path, role: &str, keys: &'a Keys) -> Result<Self> {
        let store = Self::new(backups, role, keys);
        create_private_dir(backups)?;
        create_private_dir(&backups.join(role))?;
        create_private_dir(&store.snapshots)?;
        create_private_dir(&store.attachments)?;
        Ok(store)
    }

    /// Snapshots, oldest first, as [`list`] finds them.
    pub fn list(&self) -> Result<Vec<SnapshotInfo>> {
        listed(&self.snapshots, &self.attachments)
    }

    fn snapshot_path(&self, id: &str) -> PathBuf {
        self.snapshots.join(format!("{id}{SNAPSHOT_SUFFIX}"))
    }

    /// Reads a snapshot. One with format 2 metadata must be the very file
    /// that the metadata names.
    pub fn read_snapshot(&self, path: &Path) -> Result<Value> {
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if let Sidecar::Current(meta) = Sidecar::read(&meta_path(path)) {
            ensure_digest(&data, &meta.snapshot_sha256, path)?;
        }
        let plain = self
            .keys
            .decrypt(&data)
            .with_context(|| format!("decrypting {}", path.display()))?;
        serde_json::from_slice(&plain)
            .with_context(|| format!("{} does not hold JSON", path.display()))
    }

    /// The file that holds an attachment's content, and the digest it must
    /// have.
    fn attachment_file<'r>(
        &self,
        attachment: &'r SnapshotAttachment,
    ) -> Result<(PathBuf, Option<&'r str>)> {
        let id = &attachment.attachment_id;
        match &attachment.storage {
            AttachmentStorage::Omitted => {
                bail!("the backup holds only a record of attachment {id}, not its content")
            }
            AttachmentStorage::Legacy => {
                Ok((self.attachments.join(attachment_file_name(id)?), None))
            }
            AttachmentStorage::Snapshot {
                snapshot_id,
                ciphertext_sha256,
            } => Ok((
                self.attachments
                    .join(path_component(snapshot_id)?)
                    .join(attachment_file_name(id)?),
                Some(ciphertext_sha256),
            )),
        }
    }

    /// An attachment's content, from the file its snapshot recorded.
    pub fn read_snapshot_attachment(&self, attachment: &SnapshotAttachment) -> Result<Vec<u8>> {
        let (path, digest) = self.attachment_file(attachment)?;
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if let Some(digest) = digest {
            ensure_digest(&data, digest, &path)?;
        }
        self.keys
            .decrypt(&data)
            .with_context(|| format!("decrypting {}", path.display()))
    }

    fn group(&self, origin: &VaultOrigin) -> Group {
        Group {
            origin: origin.id(),
            encryption: self.keys.encryption_id(),
        }
    }

    /// Records in a snapshot's encrypted bookkeeping which vault it holds and
    /// whom it is encrypted to.
    fn add_provenance(&self, document: &mut Value, origin: &VaultOrigin) -> Result<()> {
        let block = document
            .get_mut(BLOCK)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("the snapshot document has no {BLOCK} block"))?;
        block.insert(
            "origin".into(),
            json!({
                "apiUrl": origin.api_url,
                "accountId": origin.account_id,
                "organizationId": origin.organization_id,
            }),
        );
        block.insert(
            "encryption".into(),
            json!({"recipients": self.keys.recipient_names()}),
        );
        Ok(())
    }

    /// The newest snapshot, when it already holds `document` and every
    /// attachment file it promises opens, so that a run needs no new one.
    /// `document` is what the run would write, with every attachment record
    /// [`AttachmentStorage::Omitted`]; `attachments` says whether it would
    /// store their content.
    ///
    /// Anything uncertain gives `None`: a needless snapshot costs only space.
    /// So do keys that can't read what they write, since they can't check.
    pub fn unchanged(
        &self,
        origin: &VaultOrigin,
        document: &Value,
        attachments: bool,
    ) -> Result<Option<PathBuf>> {
        if !self.keys.reads_own() {
            return Ok(None);
        }
        let Some(latest) = self.list()?.pop() else {
            return Ok(None);
        };
        // Two empty vaults are alike, and so are two vaults read with other
        // keys; neither stands in for this one.
        if latest.meta.and_then(|meta| meta.group) != Some(self.group(origin)) {
            return Ok(None);
        }
        let mut expected = document.clone();
        self.add_provenance(&mut expected, origin)?;
        let holds = self.holds(&latest.path, &expected, attachments);
        Ok(holds.unwrap_or(false).then_some(latest.path))
    }

    /// Whether the snapshot at `path` is `expected`, apart from what differs
    /// between two snapshots of one vault, with every attachment file intact.
    fn holds(&self, path: &Path, expected: &Value, attachments: bool) -> Result<bool> {
        let stored = self.read_snapshot(path)?;
        if logical(&stored) != logical(expected) {
            return Ok(false);
        }
        let id = snapshot_id_of(path).unwrap_or_default();
        for attachment in snapshot::parse(&stored)?
            .items()
            .iter()
            .flat_map(|item| &item.attachments)
        {
            let intact = match &attachment.storage {
                AttachmentStorage::Omitted => !attachments,
                AttachmentStorage::Snapshot { snapshot_id, .. } => {
                    attachments
                        && snapshot_id == id
                        && self.read_snapshot_attachment(attachment).is_ok()
                }
                AttachmentStorage::Legacy => false,
            };
            if !intact {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Starts a new snapshot of `origin`, under an id no other snapshot has
    /// had.
    pub fn begin_snapshot(
        &self,
        origin: &VaultOrigin,
        now: OffsetDateTime,
    ) -> Result<SnapshotWriter<'_>> {
        if self.keys.recipients.is_empty() {
            bail!("these backup keys only read backups");
        }
        // Ids and the times they are read back as are in UTC.
        let now = now.to_offset(UtcOffset::UTC);
        let base = format!(
            "vault-{}",
            now.format(ID_TIME)
                .context("formatting the snapshot name")?
        );
        for counter in 0.. {
            let id = match counter {
                0 => base.clone(),
                _ => format!("{base}-{counter}"),
            };
            // Format 1 snapshots have no directory, but their ids stay taken.
            let meta = self.snapshots.join(format!("{id}{META_SUFFIX}"));
            if self.snapshot_path(&id).try_exists()? || meta.try_exists()? {
                continue;
            }
            let directory = self.attachments.join(&id);
            match fs::create_dir(&directory) {
                // Only one writer can create it, so the id is this one's alone.
                Ok(()) => {
                    restrict(&directory);
                    let writer = SnapshotWriter {
                        store: self,
                        id,
                        origin: origin.clone(),
                        created: now,
                        directory,
                        attachment_ciphertext_digests: Mutex::default(),
                        document_written: false,
                        finished: false,
                    };
                    sync_dir(&self.attachments)?;
                    return Ok(writer);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", directory.display()));
                }
            }
        }
        unreachable!("the counter is unbounded")
    }

    /// Reads back a file just written, and decrypts it when the keys can, so
    /// that a snapshot never counts on a file that doesn't hold what it should.
    fn check_written(&self, path: &Path, ciphertext: &[u8], plaintext: &[u8]) -> Result<()> {
        let stored = fs::read(path).with_context(|| format!("reading back {}", path.display()))?;
        let intact = stored == ciphertext
            && (!self.keys.reads_own()
                || (self.keys.decrypt(&stored)).is_ok_and(|read| read == plaintext));
        if !intact {
            bail!("{} does not read back as it was written", path.display());
        }
        Ok(())
    }

    /// Removes the snapshots of `origin`, made with the current keys, that
    /// are past their retention. Thinning older days to one snapshot bounds
    /// the count however often a changing vault is backed up.
    ///
    /// Snapshots of other vaults or keys, of format 1, or never finished all
    /// stay: they may be the only way back to something this run can't see.
    /// While any metadata can't be read, which snapshots it describes is
    /// unknown, so nothing is removed.
    pub fn prune(
        &self,
        origin: &VaultOrigin,
        retention: &Retention,
        now: OffsetDateTime,
    ) -> Result<PruneReport> {
        let unreadable = unreadable_metadata(&self.snapshots)?;
        if !unreadable.is_empty() {
            return Err(unreadable_error(&unreadable, "it pruned no snapshot"));
        }
        let group = Some(self.group(origin));
        let snapshots: Vec<Found> = scan(&self.snapshots, &self.attachments)?
            .into_iter()
            .filter(|found| found.sidecar.meta().and_then(|meta| meta.group) == group)
            .collect();
        let cutoff = now - Duration::days(i64::from(retention.days));
        let recent = now - Duration::days(1);
        let newest = snapshots
            .len()
            .saturating_sub(retention.keep_at_least.max(1));
        let newest_of_day: BTreeMap<time::Date, usize> = (snapshots.iter().enumerate())
            .map(|(index, snapshot)| (snapshot.created.date(), index))
            .collect();
        let mut report = PruneReport::default();
        for (index, snapshot) in snapshots.iter().enumerate() {
            let in_retention = snapshot.created >= cutoff
                && newest_of_day.get(&snapshot.created.date()) == Some(&index);
            if index >= newest || snapshot.created >= recent || in_retention {
                continue;
            }
            report.attachments_removed += self.remove(snapshot)?;
            report.snapshots_removed += 1;
        }
        Ok(report)
    }

    /// Removes a format 2 snapshot and returns how many attachment files went
    /// with it. The metadata goes first, so that a crash part way leaves a
    /// snapshot that no longer counts, not one that lacks its files.
    fn remove(&self, snapshot: &Found) -> Result<usize> {
        for path in [meta_path(&snapshot.path), snapshot.path.clone()] {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        sync_dir(&self.snapshots)?;
        let directory = self.attachments.join(&snapshot.id);
        let files = paths_in(&directory)?.len();
        if directory.exists() {
            fs::remove_dir_all(&directory)
                .with_context(|| format!("removing {}", directory.display()))?;
        }
        Ok(files)
    }
}

/// `document` without what differs between two snapshots of one vault: when
/// each was made, and where each keeps its attachment files.
fn logical(document: &Value) -> Value {
    let mut document = document.clone();
    if let Some(block) = document.get_mut(BLOCK).and_then(Value::as_object_mut) {
        block.shift_remove("createdAt");
        let records = block.get_mut("attachments").and_then(Value::as_array_mut);
        for record in records
            .into_iter()
            .flatten()
            .filter_map(Value::as_object_mut)
        {
            for key in ["stored", "snapshotId", "ciphertextSha256"] {
                record.shift_remove(key);
            }
        }
    }
    document
}

/// A snapshot being written. Its files count once [`SnapshotWriter::finish`]
/// has written the metadata. Dropped before that, it removes them.
pub struct SnapshotWriter<'s> {
    store: &'s Store<'s>,
    id: String,
    origin: VaultOrigin,
    created: OffsetDateTime,
    directory: PathBuf,
    attachment_ciphertext_digests: Mutex<BTreeMap<String, String>>,
    document_written: bool,
    finished: bool,
}

impl SnapshotWriter<'_> {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Encrypts one attachment's content into a file of this snapshot.
    pub fn write_attachment(
        &self,
        attachment_id: &str,
        plaintext: &[u8],
    ) -> Result<AttachmentStorage> {
        let path = self.directory.join(attachment_file_name(attachment_id)?);
        let ciphertext = self.store.keys.encrypt(plaintext)?;
        write_private_new(&path, &ciphertext)
            .with_context(|| format!("writing {}", path.display()))?;
        let digest = sha256_hex(&ciphertext);
        (self
            .attachment_ciphertext_digests
            .lock()
            .unwrap_or_else(PoisonError::into_inner))
        .insert(attachment_id.to_string(), digest.clone());
        self.store.check_written(&path, &ciphertext, plaintext)?;
        Ok(AttachmentStorage::Snapshot {
            snapshot_id: self.id.clone(),
            ciphertext_sha256: digest,
        })
    }

    /// Writes `document`, built from the records `write_attachment` returned,
    /// and then the metadata that makes the snapshot count.
    pub fn finish(mut self, mut document: Value) -> Result<PathBuf> {
        let created_at = self
            .created
            .format(&Rfc3339)
            .context("formatting the snapshot time")?;
        snapshot::stamp(&mut document, &created_at);
        self.store.add_provenance(&mut document, &self.origin)?;
        let attachment_files = self.check_records(&document)?;
        let items = document
            .get("items")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let plaintext = serde_json::to_vec_pretty(&document)?;
        let ciphertext = self.store.keys.encrypt(&plaintext)?;
        let path = self.store.snapshot_path(&self.id);
        // Each step is durable before the next, so that after a crash no
        // snapshot is found without its files, nor metadata without both.
        sync_dir(&self.directory)?;
        write_private_new(&path, &ciphertext)
            .with_context(|| format!("writing {}", path.display()))?;
        self.document_written = true;
        self.store.check_written(&path, &ciphertext, &plaintext)?;
        sync_dir(&self.store.snapshots)?;
        let meta = MetaFile {
            format: META_FORMAT,
            created_at,
            items,
            attachment_files,
            origin: self.origin.id(),
            encryption: self.store.keys.encryption_id(),
            snapshot_sha256: sha256_hex(&ciphertext),
        };
        let meta_path = meta_path(&path);
        write_private_new(&meta_path, &serde_json::to_vec_pretty(&meta)?)
            .with_context(|| format!("writing {}", meta_path.display()))?;
        self.finished = true;
        sync_dir(&self.store.snapshots)?;
        Ok(path)
    }

    /// Checks that `document` names exactly the attachment files this writer
    /// stored, and returns how many there are.
    fn check_records(&self, document: &Value) -> Result<usize> {
        let files = self
            .attachment_ciphertext_digests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let restorable = snapshot::parse(document)?;
        let mut named = BTreeSet::new();
        for attachment in restorable.items().iter().flat_map(|item| &item.attachments) {
            let id = &attachment.attachment_id;
            let stored_here = match &attachment.storage {
                AttachmentStorage::Omitted => true,
                AttachmentStorage::Snapshot {
                    snapshot_id,
                    ciphertext_sha256,
                } => {
                    *snapshot_id == self.id
                        && files.get(id) == Some(ciphertext_sha256)
                        && named.insert(id)
                }
                AttachmentStorage::Legacy => false,
            };
            if !stored_here {
                bail!(
                    "snapshot {} names a file it did not store for attachment {id}",
                    self.id
                );
            }
        }
        if named.len() != files.len() {
            bail!(
                "snapshot {} stored {} that it does not name",
                self.id,
                count(files.len() - named.len(), "attachment file")
            );
        }
        Ok(files.len())
    }
}

impl Drop for SnapshotWriter<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The id was this writer's alone, so all that carries it is its own.
        // A document left without its directory would pass for a format 1
        // snapshot, so the directory goes only once the document is gone.
        let gone = !self.document_written
            || (match fs::remove_file(self.store.snapshot_path(&self.id)) {
                Ok(()) => true,
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            } && sync_dir(&self.store.snapshots).is_ok());
        if gone {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn meta_path(snapshot: &Path) -> PathBuf {
    let stem = snapshot_id_of(snapshot).unwrap_or(file_name(snapshot));
    snapshot.with_file_name(format!("{stem}{META_SUFFIX}"))
}

/// The time and counter in an id `begin_snapshot` gave, and `None` for any
/// other name, so that pruning never removes a file vault-mirror didn't write.
fn parse_snapshot_id(id: &str) -> Option<(OffsetDateTime, u64)> {
    let rest = id.strip_prefix("vault-")?;
    let (stamp, counter) = rest.split_at_checked(16)?;
    let counter = match counter.strip_prefix('-') {
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse().ok()?
        }
        None if counter.is_empty() => 0,
        _ => return None,
    };
    let created = PrimitiveDateTime::parse(stamp, ID_TIME).ok()?.assume_utc();
    Some((created, counter))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::{
        node::Node,
        vault::{Item, Scope, Vault},
    };

    fn passphrase(text: &str) -> Protection {
        Protection::Passphrase(text.to_string())
    }

    fn origin(account: &str) -> VaultOrigin {
        VaultOrigin {
            api_url: "https://vault.example/api".into(),
            account_id: account.into(),
            organization_id: None,
        }
    }

    /// A vault with one note that holds `secret`, or no items for `None`.
    fn vault(secret: Option<&str>) -> Vault {
        let items = secret.map(|secret| Item {
            id: "item-1".into(),
            folder_id: None,
            collection_ids: vec![],
            revision_date: Some("2026-01-01T00:00:00.000000Z".into()),
            creation_date: None,
            in_trash: false,
            archived: false,
            archived_date: None,
            content: BTreeMap::from([
                ("type".to_string(), Node::Number(2.into())),
                ("name".to_string(), Node::secret("Note")),
                ("notes".to_string(), Node::secret(secret)),
            ]),
            attachments: vec![],
            wrapped_key: None,
        });
        Vault {
            items: items.into_iter().collect(),
            ..Vault::default()
        }
    }

    fn record(id: &str, storage: AttachmentStorage) -> SnapshotAttachment {
        SnapshotAttachment {
            item_id: "item-1".into(),
            attachment_id: id.into(),
            file_name: format!("{id}.txt"),
            size: 81,
            storage,
        }
    }

    /// Writes a snapshot of the note holding `secret`, with `files` as its
    /// attachments.
    fn write(
        store: &Store,
        origin: &VaultOrigin,
        at: OffsetDateTime,
        secret: &str,
        files: &[(&str, &[u8])],
    ) -> PathBuf {
        let writer = store.begin_snapshot(origin, at).unwrap();
        let records: Vec<_> = files
            .iter()
            .map(|(id, data)| record(id, writer.write_attachment(id, data).unwrap()))
            .collect();
        let document = snapshot::build(&vault(Some(secret)), &Scope::Personal, &records).unwrap();
        writer.finish(document).unwrap()
    }

    /// What a run that finds the note holding `secret` compares with the
    /// newest snapshot.
    fn planned(secret: Option<&str>, files: &[&str]) -> Value {
        let records: Vec<_> = (files.iter())
            .map(|id| record(id, AttachmentStorage::Omitted))
            .collect();
        snapshot::build(&vault(secret), &Scope::Personal, &records).unwrap()
    }

    /// Writes a snapshot as vault-mirror 0.5 did: attachment files shared by
    /// every snapshot, and metadata with a salt and fingerprint.
    fn write_legacy(store: &Store, at: OffsetDateTime, files: &[(&str, &[u8])]) -> PathBuf {
        for (id, data) in files {
            let path = store.attachments.join(format!("{id}.age"));
            write_private(&path, &store.keys.encrypt(data).unwrap()).unwrap();
        }
        let id = format!("vault-{}", at.format(ID_TIME).unwrap());
        let path = store.snapshot_path(&id);
        let document =
            json!({"encrypted": false, "folders": [], "items": [], "vaultMirror": {"format": 1}});
        write_private(
            &path,
            &store.keys.encrypt(document.to_string().as_bytes()).unwrap(),
        )
        .unwrap();
        let meta = json!({
            "format": 1,
            "created_at": at.format(&Rfc3339).unwrap(),
            "items": 0,
            "salt": "00112233445566778899aabbccddeeff",
            "fingerprint": "ab".repeat(32),
            "attachments": files.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            "vault": "user",
        });
        write_private(&meta_path(&path), meta.to_string().as_bytes()).unwrap();
        path
    }

    fn identity_file(dir: &Path, name: &str, identities: &[&age::x25519::Identity]) -> PathBuf {
        let path = dir.join(name);
        let text: String = (identities.iter())
            .map(|identity| format!("# comment\n{}\n", identity.to_string().expose_secret()))
            .collect();
        fs::write(&path, text).unwrap();
        path
    }

    fn recipients(identity: &age::x25519::Identity) -> Protection {
        Protection::Recipients(vec![identity.to_public().to_string()])
    }

    fn json_file(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
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

    /// The attachment directory of the snapshot at `path`.
    fn directory_of(store: &Store, path: &Path) -> PathBuf {
        store.attachments.join(snapshot_id_of(path).unwrap())
    }

    fn files_of(store: &Store, path: &Path) -> Vec<SnapshotAttachment> {
        let document = store.read_snapshot(path).unwrap();
        (snapshot::parse(&document).unwrap().items().iter())
            .flat_map(|item| item.attachments.clone())
            .collect()
    }

    #[test]
    fn a_snapshot_round_trips_and_is_not_stored_in_the_clear() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("backup passphrase")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 02:30:00 UTC);

        let path = write(
            &store,
            &origin("user"),
            now,
            "hunter2",
            &[("att-1", b"file hunter3")],
        );
        assert_eq!(path.file_name().unwrap(), "vault-20261014T023000Z.json.age");
        for entry in walk(dir.path()) {
            let raw = fs::read(&entry).unwrap();
            for secret in [&b"hunter2"[..], b"hunter3", b"user", b"vault.example"] {
                assert!(
                    !contains(&raw, secret),
                    "{} holds {} in the clear",
                    entry.display(),
                    String::from_utf8_lossy(secret)
                );
            }
        }
        let read = store.read_snapshot(&path).unwrap();
        assert_eq!(read["items"][0]["notes"], "hunter2");
        assert_eq!(read[BLOCK]["createdAt"], "2026-10-14T02:30:00Z");
        assert_eq!(read[BLOCK]["origin"]["accountId"], "user");
        assert_eq!(
            read[BLOCK]["encryption"]["recipients"],
            json!(keys.recipient_names())
        );
        let files = files_of(&store, &path);
        let [attachment] = &files[..] else {
            panic!("one attachment")
        };
        assert_eq!(
            store.read_snapshot_attachment(attachment).unwrap(),
            b"file hunter3"
        );

        // A later run with the same passphrase reads what an earlier run wrote.
        let reopened = Keys::open(dir.path(), &passphrase("backup passphrase")).unwrap();
        let store = Store::open(dir.path(), "source", &reopened).unwrap();
        assert_eq!(store.read_snapshot(&path).unwrap(), read);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_private_store_works_below_a_traversal_only_ancestor() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join("traverse-only");
        let backups = ancestor.join("backups");
        fs::create_dir_all(&backups).unwrap();
        fs::set_permissions(&backups, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o311)).unwrap();
        let read_denied = fs::read_dir(&ancestor).is_err();
        let result = (|| -> Result<()> {
            let keys = Keys::open(&backups, &passphrase("synthetic backup passphrase"))?;
            Store::open(&backups, "source", &keys)?;
            Ok(())
        })();
        let ancestor_mode = fs::metadata(&ancestor).unwrap().permissions().mode() & 0o777;
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(ancestor_mode, 0o311);
        if read_denied {
            result.unwrap();
        }
    }

    #[test]
    fn a_different_passphrase_is_rejected_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        Keys::open(dir.path(), &passphrase("first")).unwrap();
        let error = Keys::open(dir.path(), &passphrase("second")).err().unwrap();
        assert!(format!("{error:#}").contains("different one"), "{error:#}");
        let error = Keys::from_key_file(dir.path(), "second").err().unwrap();
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
        let t0 = datetime!(2026-10-14 00:00 UTC);
        let written = [(&first, "first"), (&second, "second")].map(|(keys, role)| {
            let store = Store::open(dir.path(), role, keys).unwrap();
            write(&store, &origin(role), t0, role, &[])
        });

        let kept = Keys::from_key_file(dir.path(), "p").unwrap();
        for (path, role) in written.iter().zip(["first", "second"]) {
            let store = Store::new(dir.path(), role, &kept);
            assert_eq!(
                store.read_snapshot(path).unwrap()["items"][0]["notes"],
                role
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key = fs::metadata(dir.path().join(KEY_FILE)).unwrap();
            assert_eq!(key.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_missing_key_is_not_replaced_in_a_directory_that_holds_backups() {
        let dir = tempfile::tempdir().unwrap();
        // What a run writes before it opens the key doesn't count.
        for name in RUN_FILES {
            fs::write(dir.path().join(name), b"{}").unwrap();
        }
        fs::create_dir_all(dir.path().join("source/snapshots")).unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        write(
            &store,
            &origin("user"),
            datetime!(2026-10-14 00:00 UTC),
            "a",
            &[],
        );
        let elsewhere = tempfile::tempdir().unwrap();
        let original = elsewhere.path().join(KEY_FILE);
        fs::rename(dir.path().join(KEY_FILE), &original).unwrap();

        let error = Keys::open(dir.path(), &passphrase("p")).err().unwrap();
        assert!(format!("{error:#}").contains("is missing"), "{error:#}");
        assert!(!dir.path().join(KEY_FILE).exists());
        assert!(Keys::from_key_file(dir.path(), "p").is_err());
        assert!(!dir.path().join(KEY_FILE).exists());

        // Any other file is a sign of earlier backups.
        fs::remove_dir_all(dir.path().join("source")).unwrap();
        fs::write(dir.path().join("unfinished-pull"), b"").unwrap();
        assert!(Keys::open(dir.path(), &passphrase("p")).is_err());
        assert!(!dir.path().join(KEY_FILE).exists());

        fs::rename(&original, dir.path().join(KEY_FILE)).unwrap();
        assert!(Keys::open(dir.path(), &passphrase("p")).is_ok());
    }

    #[test]
    fn reading_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("backups");
        assert!(Keys::from_key_file(&backups, "p").is_err());
        assert_eq!(list(&backups, "source").unwrap(), []);
        let identity = age::x25519::Identity::generate();
        let file = identity_file(dir.path(), "identity.txt", &[&identity]);
        let keys = Keys::from_identity_file(&file).unwrap();
        assert_eq!(Store::new(&backups, "source", &keys).list().unwrap(), []);
        assert!(!backups.exists());
        let error = Store::new(&backups, "source", &keys)
            .begin_snapshot(&origin("user"), datetime!(2026-10-14 00:00 UTC))
            .err()
            .unwrap();
        assert!(error.to_string().contains("only read"), "{error}");
        assert!(!backups.exists());
    }

    #[test]
    fn every_identity_in_an_identity_file_is_tried() {
        let dir = tempfile::tempdir().unwrap();
        let first = age::x25519::Identity::generate();
        let second = age::x25519::Identity::generate();
        let keys = Keys::open(dir.path(), &recipients(&second)).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let path = write(
            &store,
            &origin("user"),
            datetime!(2026-10-14 00:00 UTC),
            "a",
            &[("att-1", b"x")],
        );

        let file = identity_file(dir.path(), "identities.txt", &[&first, &second]);
        let keys = Keys::from_identity_file(&file).unwrap();
        let store = Store::new(dir.path(), "source", &keys);
        assert_eq!(
            store.read_snapshot(&path).unwrap()["items"][0]["notes"],
            "a"
        );
        assert_eq!(
            store
                .read_snapshot_attachment(&files_of(&store, &path)[0])
                .unwrap(),
            b"x"
        );
    }

    #[test]
    fn after_a_key_change_new_snapshots_open_with_the_new_key_alone() {
        let dir = tempfile::tempdir().unwrap();
        let old = age::x25519::Identity::generate();
        let new = age::x25519::Identity::generate();
        let t0 = datetime!(2026-10-14 00:00 UTC);
        let old_keys = Keys::open(dir.path(), &recipients(&old)).unwrap();
        let old_store = Store::open(dir.path(), "source", &old_keys).unwrap();
        write_legacy(&old_store, t0, &[("att-1", b"same file")]);
        let before = write(
            &old_store,
            &origin("user"),
            t0,
            "a",
            &[("att-1", b"same file")],
        );

        let new_keys = Keys::open(dir.path(), &recipients(&new)).unwrap();
        let new_store = Store::open(dir.path(), "source", &new_keys).unwrap();
        let after = write(
            &new_store,
            &origin("user"),
            t0 + Duration::hours(1),
            "a",
            &[("att-1", b"same file")],
        );

        let file = identity_file(dir.path(), "new.txt", &[&new]);
        let reader = Keys::from_identity_file(&file).unwrap();
        let store = Store::new(dir.path(), "source", &reader);
        let files = files_of(&store, &after);
        let [attachment] = &files[..] else {
            panic!("one attachment")
        };
        assert_eq!(
            store.read_snapshot_attachment(attachment).unwrap(),
            b"same file"
        );
        // The old snapshot still needs the old key, and the passphrase key
        // can be combined with it.
        assert!(store.read_snapshot(&before).is_err());
        let both = Keys::from_identity_file(&file)
            .unwrap()
            .with_identity_file(&identity_file(dir.path(), "old.txt", &[&old]))
            .unwrap();
        assert!(
            Store::new(dir.path(), "source", &both)
                .read_snapshot(&before)
                .is_ok()
        );
    }

    #[test]
    fn metadata_in_the_clear_holds_no_verifier_of_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let path = write(
            &store,
            &origin("user"),
            datetime!(2026-10-14 00:00 UTC),
            "a",
            &[("att-1", b"x")],
        );
        let meta = json_file(&meta_path(&path));
        let mut keys: Vec<&str> = meta
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "attachment_files",
                "created_at",
                "encryption",
                "format",
                "items",
                "origin",
                "snapshot_sha256"
            ]
        );
        assert_eq!(
            meta["snapshot_sha256"],
            sha256_hex(&fs::read(&path).unwrap())
        );
        let listed = store.list().unwrap();
        assert_eq!(
            listed[0]
                .meta
                .as_ref()
                .map(|meta| (meta.format, meta.items, meta.attachment_files)),
            Some((2, 1, 1))
        );
    }

    #[test]
    fn migration_removes_every_fingerprint_and_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let t0 = datetime!(2026-10-14 00:00 UTC);
        let mut legacy = Vec::new();
        for role in ["source", "dest"] {
            let store = Store::open(dir.path(), role, &keys).unwrap();
            legacy.push(write_legacy(&store, t0, &[("att-1", b"x")]));
            write(&store, &origin("user"), t0 + Duration::hours(1), "a", &[]);
        }
        // What a run killed while writing metadata leaves behind.
        let leftover = dir.path().join("dest/snapshots/.tmp-abc");
        fs::copy(meta_path(&legacy[1]), &leftover).unwrap();
        let encrypted = |path: &PathBuf| path.extension().is_some_and(|e| e == "age");
        let before_files: Vec<(PathBuf, Vec<u8>)> = (walk(dir.path()).into_iter())
            .filter(encrypted)
            .map(|path| (fs::read(&path).unwrap(), path))
            .map(|(data, path)| (path, data))
            .collect();
        let before: Vec<SnapshotInfo> = list(dir.path(), "source").unwrap();

        migrate_metadata(dir.path()).unwrap();
        for path in walk(dir.path()).into_iter().filter(|path| !encrypted(path)) {
            let raw = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
            assert!(
                !raw.contains("salt")
                    && !raw.contains("fingerprint")
                    && !raw.contains(&"ab".repeat(32)),
                "{} still holds a fingerprint",
                path.display()
            );
        }
        let meta = json_file(&meta_path(&legacy[0]));
        assert_eq!(
            meta,
            json!({"format": 1, "created_at": "2026-10-14T00:00:00Z", "items": 0, "attachments": ["att-1"], "vault": "user"})
        );
        assert_eq!(json_file(&leftover), json_file(&meta_path(&legacy[1])));
        for (path, data) in &before_files {
            assert_eq!(&fs::read(path).unwrap(), data, "{}", path.display());
        }
        assert_eq!(list(dir.path(), "source").unwrap(), before);
        migrate_metadata(dir.path()).unwrap();
        assert_eq!(json_file(&meta_path(&legacy[0])), meta);
    }

    #[test]
    fn metadata_this_version_cant_read_is_kept_and_stops_migration_and_pruning() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-15 12:00 UTC);
        let old = write(
            &store,
            &origin("user"),
            now - Duration::days(40),
            "old",
            &[],
        );
        let legacy = write_legacy(&store, now - Duration::days(39), &[]);
        let damaged = write(
            &store,
            &origin("user"),
            now - Duration::days(20),
            "mid",
            &[],
        );
        write(&store, &origin("user"), now, "new", &[]);
        let damaged_meta = meta_path(&damaged);
        let future = store.snapshots.join("vault-20261001T000000Z.meta.json");
        fs::write(&damaged_meta, b"{\"salt\": \"SALT-VALUE\"").unwrap();
        fs::write(&future, br#"{"format": 3}"#).unwrap();

        let error = migrate_metadata(dir.path()).unwrap_err().to_string();
        assert!(
            error.contains(&damaged_meta.display().to_string())
                && error.contains(&future.display().to_string()),
            "{error}"
        );
        assert!(!error.contains("SALT-VALUE"), "{error}");
        assert_eq!(
            fs::read(&damaged_meta).unwrap(),
            b"{\"salt\": \"SALT-VALUE\""
        );
        assert!(json_file(&meta_path(&legacy)).get("salt").is_none());

        let retention = Retention {
            days: 30,
            keep_at_least: 1,
        };
        let error = store.prune(&origin("user"), &retention, now).unwrap_err();
        assert!(error.to_string().contains("pruned no snapshot"), "{error}");
        assert!(old.exists() && damaged.exists());
    }

    #[test]
    fn format_1_snapshots_stay_readable_and_are_never_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-15 12:00 UTC);
        let legacy = write_legacy(&store, now - Duration::days(400), &[("att-1", b"old file")]);
        fs::remove_file(meta_path(&write_legacy(
            &store,
            now - Duration::days(300),
            &[],
        )))
        .unwrap();
        for days in [90, 60, 0] {
            write(
                &store,
                &origin("user"),
                now - Duration::days(days),
                "a",
                &[],
            );
        }
        let retention = Retention {
            days: 30,
            keep_at_least: 1,
        };
        let report = store.prune(&origin("user"), &retention, now).unwrap();
        assert_eq!(report.snapshots_removed, 2);
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].path, legacy);
        assert_eq!(listed[0].meta.as_ref().unwrap().format, 1);
        assert_eq!(listed[1].meta, None);
        assert_eq!(store.read_snapshot(&legacy).unwrap()[BLOCK]["format"], 1);
        assert_eq!(
            store
                .read_snapshot_attachment(&record("att-1", AttachmentStorage::Legacy))
                .unwrap(),
            b"old file"
        );
    }

    #[test]
    fn pruning_thins_only_the_snapshots_of_this_vault_and_these_keys() {
        let dir = tempfile::tempdir().unwrap();
        let old = age::x25519::Identity::generate();
        let now = datetime!(2026-10-15 12:00 UTC);
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let old_keys = Keys::open(dir.path(), &recipients(&old)).unwrap();
        let old_store = Store::open(dir.path(), "source", &old_keys).unwrap();
        let other_keys = write(
            &old_store,
            &origin("user"),
            now - Duration::days(40),
            "a",
            &[],
        );

        let other_vault = write(&store, &origin("org"), now - Duration::days(41), "b", &[]);
        let mine = write(
            &store,
            &origin("user"),
            now - Duration::days(42),
            "c",
            &[("att-1", b"x"), ("att-2", b"y")],
        );
        write(&store, &origin("user"), now, "d", &[("att-1", b"x")]);
        let report = store
            .prune(
                &origin("user"),
                &Retention {
                    days: 30,
                    keep_at_least: 1,
                },
                now,
            )
            .unwrap();
        assert_eq!(
            report,
            PruneReport {
                snapshots_removed: 1,
                attachments_removed: 2
            }
        );
        assert!(!mine.exists() && !meta_path(&mine).exists());
        assert!(!directory_of(&store, &mine).exists());
        assert!(other_keys.exists() && other_vault.exists());
    }

    #[test]
    fn pruning_keeps_recent_snapshots_and_the_newest_ones() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 00:00:00 UTC);
        for days in [90, 60, 10, 1] {
            let at = now - Duration::days(days);
            write(
                &store,
                &origin("user"),
                at,
                &days.to_string(),
                &[("att", b"x")],
            );
        }
        let retention = Retention {
            days: 30,
            keep_at_least: 1,
        };
        let report = store.prune(&origin("user"), &retention, now).unwrap();
        assert_eq!(
            report,
            PruneReport {
                snapshots_removed: 2,
                attachments_removed: 2
            }
        );
        assert_eq!(store.list().unwrap().len(), 2);

        // Far in the future everything is old, and the newest still stays.
        let later = now + Duration::days(365);
        let retention = Retention {
            days: 30,
            keep_at_least: 0,
        };
        let report = store.prune(&origin("user"), &retention, later).unwrap();
        assert_eq!(report.snapshots_removed, 1);
        let listed = store.list().unwrap();
        let [newest] = &listed[..] else {
            panic!("one snapshot")
        };
        assert_eq!(newest.created, now - Duration::days(1));
        assert_eq!(
            store
                .read_snapshot_attachment(&files_of(&store, &newest.path)[0])
                .unwrap(),
            b"x"
        );
    }

    #[test]
    fn hourly_snapshots_thin_out_to_one_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 00:00:00 UTC);
        // A vault that changed every hour for a week.
        for hours in (1..=7 * 24).rev() {
            let at = now - Duration::hours(hours);
            write(&store, &origin("user"), at, &hours.to_string(), &[]);
        }
        let retention = Retention {
            days: 5,
            keep_at_least: 10,
        };
        store.prune(&origin("user"), &retention, now).unwrap();
        let kept = store.list().unwrap();
        // All 24 of the last day, then the newest of each of the 4 days
        // before it that are within 5 days.
        assert_eq!(kept.len(), 24 + 4);
        assert_eq!(kept.last().unwrap().created, now - Duration::hours(1));
        let older: Vec<_> = kept
            .iter()
            .filter(|s| s.created < now - Duration::days(1))
            .collect();
        assert!(
            older.iter().all(|s| s.created.hour() == 23),
            "the newest of each day"
        );

        // Pruning again removes nothing.
        assert_eq!(
            store.prune(&origin("user"), &retention, now).unwrap(),
            PruneReport::default()
        );
    }

    #[test]
    fn an_unfinished_snapshot_is_never_the_newest_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-15 12:00 UTC);
        let complete = write(&store, &origin("user"), now - Duration::days(40), "a", &[]);
        // A run killed after writing its document, before the metadata.
        let unfinished = write(&store, &origin("user"), now, "b", &[("att-1", b"x")]);
        fs::remove_file(meta_path(&unfinished)).unwrap();

        let listed: Vec<PathBuf> = store.list().unwrap().into_iter().map(|s| s.path).collect();
        assert_eq!(listed, std::slice::from_ref(&complete));
        assert_eq!(list(dir.path(), "source").unwrap()[0].path, complete);
        // The older snapshot is still the newest complete one, so it stays.
        let retention = Retention {
            days: 30,
            keep_at_least: 1,
        };
        assert_eq!(
            store.prune(&origin("user"), &retention, now).unwrap(),
            PruneReport::default()
        );
        assert!(complete.exists() && unfinished.exists());
        // It can still be read by its path.
        assert_eq!(
            store.read_snapshot(&unfinished).unwrap()["items"][0]["notes"],
            "b"
        );
        assert_eq!(
            store
                .unchanged(&origin("user"), &planned(Some("a"), &[]), true)
                .unwrap(),
            Some(complete)
        );
    }

    #[test]
    fn a_writer_that_stops_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let writer = store
            .begin_snapshot(&origin("user"), datetime!(2026-10-14 00:00 UTC))
            .unwrap();
        writer.write_attachment("att-1", b"x").unwrap();
        assert!(writer.write_attachment("../escape", b"x").is_err());
        assert!(writer.write_attachment("att-1", b"again").is_err());
        drop(writer);
        assert_eq!(walk(dir.path()), [dir.path().join(KEY_FILE)]);

        // Nor does one whose document names files it did not store.
        let writer = store
            .begin_snapshot(&origin("user"), datetime!(2026-10-14 00:00 UTC))
            .unwrap();
        let stored = writer.write_attachment("att-1", b"x").unwrap();
        let elsewhere = AttachmentStorage::Snapshot {
            snapshot_id: "vault-20200101T000000Z".into(),
            ciphertext_sha256: "00".repeat(32),
        };
        for records in [vec![], vec![record("att-1", elsewhere)]] {
            let document = snapshot::build(&vault(Some("a")), &Scope::Personal, &records).unwrap();
            let writer = store
                .begin_snapshot(&origin("user"), datetime!(2026-10-14 00:00 UTC))
                .unwrap();
            writer.write_attachment("att-1", b"x").unwrap();
            assert!(writer.finish(document).is_err());
        }
        drop((writer, stored));
        assert_eq!(walk(dir.path()), [dir.path().join(KEY_FILE)]);
        assert_eq!(store.list().unwrap(), []);
    }

    #[test]
    fn a_writer_that_cannot_remove_its_document_keeps_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let mut writer = store
            .begin_snapshot(&origin("user"), datetime!(2026-10-14 00:00 UTC))
            .unwrap();
        writer.write_attachment("att-1", b"x").unwrap();
        // A document that is there and can't be removed, as a directory can't.
        fs::create_dir(store.snapshot_path(writer.id())).unwrap();
        writer.document_written = true;
        let directory = writer.directory.clone();
        drop(writer);
        // Without its directory, the document would pass for a format 1
        // snapshot and become the newest.
        assert!(directory.join("att-1.age").exists());
        assert_eq!(store.list().unwrap(), []);
    }

    #[test]
    fn snapshots_in_the_same_second_get_their_own_ids_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let now = datetime!(2026-10-14 02:30:00 UTC);
        // A format 1 snapshot already has the first id of that second.
        let legacy = write_legacy(&store, now, &[]);
        let written: Vec<PathBuf> = (0..11)
            .map(|n| write(&store, &origin("user"), now, &n.to_string(), &[]))
            .collect();
        assert_eq!(
            written[0].file_name().unwrap(),
            "vault-20261014T023000Z-1.json.age"
        );
        let listed: Vec<PathBuf> = store.list().unwrap().into_iter().map(|s| s.path).collect();
        assert_eq!(listed, [vec![legacy], written].concat());

        // Ids are in UTC, whatever the offset of the time given.
        let elsewhere = now.to_offset(time::macros::offset!(+2));
        let writer = store.begin_snapshot(&origin("user"), elsewhere).unwrap();
        assert_eq!(writer.id(), "vault-20261014T023000Z-12");
    }

    #[test]
    fn an_unchanged_vault_is_skipped_only_after_its_newest_snapshot_checks_out() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let t0 = datetime!(2026-10-14 00:00 UTC);
        let user = origin("user");
        let latest = write(&store, &user, t0, "a", &[("att-1", b"x")]);
        let same = planned(Some("a"), &["att-1"]);

        assert_eq!(
            store.unchanged(&user, &same, true).unwrap(),
            Some(latest.clone())
        );
        let changed = [
            (planned(Some("b"), &["att-1"]), true),
            (planned(Some("a"), &[]), true),
            // A run that would leave attachment content out.
            (same.clone(), false),
        ];
        for (document, attachments) in &changed {
            assert_eq!(
                store.unchanged(&user, document, *attachments).unwrap(),
                None
            );
        }
        // Another vault, even an identical one.
        assert_eq!(
            store.unchanged(&origin("other"), &same, true).unwrap(),
            None
        );

        // A damaged attachment file, or a damaged document, is no backup.
        let file = directory_of(&store, &latest).join("att-1.age");
        let original = fs::read(&file).unwrap();
        fs::write(&file, b"damaged").unwrap();
        assert_eq!(store.unchanged(&user, &same, true).unwrap(), None);
        fs::write(&file, &original).unwrap();
        assert_eq!(
            store.unchanged(&user, &same, true).unwrap(),
            Some(latest.clone())
        );
        let document = fs::read(&latest).unwrap();
        fs::write(&latest, b"damaged").unwrap();
        assert_eq!(store.unchanged(&user, &same, true).unwrap(), None);
        fs::write(&latest, &document).unwrap();

        // Nor does a newer snapshot of another vault let an older one count.
        write(&store, &origin("other"), t0 + Duration::hours(1), "a", &[]);
        assert_eq!(store.unchanged(&user, &same, true).unwrap(), None);
    }

    #[test]
    fn two_empty_vaults_are_not_the_same_vault() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let writer = store
            .begin_snapshot(&origin("first"), datetime!(2026-10-14 00:00 UTC))
            .unwrap();
        let first = writer.finish(planned(None, &[])).unwrap();
        assert_eq!(
            store
                .unchanged(&origin("first"), &planned(None, &[]), true)
                .unwrap(),
            Some(first)
        );
        assert_eq!(
            store
                .unchanged(&origin("second"), &planned(None, &[]), true)
                .unwrap(),
            None
        );
    }

    #[test]
    fn keys_that_cannot_read_their_backups_always_write_a_new_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let identity = age::x25519::Identity::generate();
        let keys = Keys::open(dir.path(), &recipients(&identity)).unwrap();
        assert!(!dir.path().join(KEY_FILE).exists());
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let path = write(
            &store,
            &origin("user"),
            datetime!(2026-10-14 00:00 UTC),
            "a",
            &[("att-1", b"x")],
        );
        assert_eq!(
            store
                .unchanged(&origin("user"), &planned(Some("a"), &["att-1"]), true)
                .unwrap(),
            None
        );
        let error = store.read_snapshot(&path).unwrap_err();
        assert!(format!("{error:#}").contains("--identity"), "{error:#}");

        let file = identity_file(dir.path(), "identity.txt", &[&identity]);
        let keys = Keys::open(dir.path(), &recipients(&identity))
            .unwrap()
            .with_identity_file(&file)
            .unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        assert_eq!(
            store.read_snapshot(&path).unwrap()["items"][0]["notes"],
            "a"
        );

        let invalid = Protection::Recipients(vec!["not-a-key".into()]);
        assert!(Keys::open(dir.path(), &invalid).is_err());
        assert!(Keys::open(dir.path(), &Protection::Recipients(vec![])).is_err());
    }

    #[test]
    fn a_file_that_is_not_the_one_recorded_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path(), &passphrase("p")).unwrap();
        let store = Store::open(dir.path(), "source", &keys).unwrap();
        let t0 = datetime!(2026-10-14 00:00 UTC);
        let first = write(&store, &origin("user"), t0, "a", &[("att-1", b"x")]);
        let second = write(
            &store,
            &origin("user"),
            t0 + Duration::hours(1),
            "b",
            &[("att-1", b"y")],
        );

        // Another snapshot's valid file in place of this one's.
        fs::copy(&second, &first).unwrap();
        let error = store.read_snapshot(&first).unwrap_err();
        assert!(
            error.to_string().contains("damaged or was replaced"),
            "{error}"
        );

        let files = files_of(&store, &second);
        let [attachment] = &files[..] else {
            panic!("one attachment")
        };
        let AttachmentStorage::Snapshot { snapshot_id, .. } = &attachment.storage else {
            panic!("stored")
        };
        let directory = store.attachments.join(snapshot_id);
        fs::copy(
            directory_of(&store, &first).join("att-1.age"),
            directory.join("att-1.age"),
        )
        .unwrap();
        assert!(store.read_snapshot_attachment(attachment).is_err());

        let omitted = record("att-1", AttachmentStorage::Omitted);
        assert!(store.read_snapshot_attachment(&omitted).is_err());
        for (snapshot_id, attachment_id) in [("..", "att-1"), ("vault-1", "../att-1"), ("a/b", "c")]
        {
            let storage = AttachmentStorage::Snapshot {
                snapshot_id: snapshot_id.into(),
                ciphertext_sha256: "00".repeat(32),
            };
            let error = store
                .read_snapshot_attachment(&record(attachment_id, storage))
                .unwrap_err();
            assert!(error.to_string().contains("refusing"), "{error}");
        }
    }
}
