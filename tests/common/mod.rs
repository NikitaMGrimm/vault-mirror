//! Shared setup for tests that run against the sandbox servers in
//! `sandbox/compose.yaml`.
#![allow(dead_code)]

pub mod fixtures;
pub mod provision;

use std::sync::{LazyLock, Mutex};

use anyhow::Result;
use provision::TestAccount;
use vault_mirror::{
    api::{ClientOptions, ServerUrls, build_http_client},
    backup::{BackupOptions, backup},
    crypto::KdfParams,
    mirror::{self, Endpoint, MirrorOptions, Outcome, Protected, Reporter},
    node::Node,
    plan::is_mirror_marker,
    store::{Keys, Store},
    vault::{Account, Draft, Item, Scope, Vault},
};

pub const PASSWORD: &str = "sandbox master password";

static WARMED_UP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

pub struct Sandbox {
    pub http: reqwest::Client,
    pub source_url: String,
    pub dest_url: String,
    /// The destination server again, behind the sandbox's TLS proxy.
    pub dest_tls_url: String,
    /// The certificate authority the TLS proxy's certificate is signed by.
    pub ca_file: std::path::PathBuf,
}

/// Connects to the sandbox. Returns `None` when it is not running, unless
/// `VAULT_MIRROR_REQUIRE_SANDBOX` is set, which CI does so that a missing
/// sandbox fails the suite instead of skipping it.
pub async fn sandbox() -> Option<Sandbox> {
    let env =
        |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    let sandbox = Sandbox {
        http: build_http_client(&ClientOptions::default()).expect("http client"),
        source_url: env("VAULT_MIRROR_SANDBOX_SOURCE", "http://127.0.0.1:18091"),
        dest_url: env("VAULT_MIRROR_SANDBOX_DEST", "http://127.0.0.1:18092"),
        dest_tls_url: env("VAULT_MIRROR_SANDBOX_DEST_TLS", "https://127.0.0.1:18444"),
        ca_file: env(
            "VAULT_MIRROR_SANDBOX_CA",
            concat!(env!("CARGO_MANIFEST_DIR"), "/sandbox/tls/ca.pem"),
        )
        .into(),
    };
    for url in [&sandbox.source_url, &sandbox.dest_url] {
        let alive = sandbox
            .http
            .get(format!("{url}/alive"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if !alive {
            if std::env::var_os("VAULT_MIRROR_REQUIRE_SANDBOX").is_some() {
                panic!("sandbox server {url} is not reachable");
            }
            eprintln!(
                "skipping: sandbox server {url} is not reachable; start it with `sandbox/up.sh`"
            );
            return None;
        }
    }
    // Bitwarden's server fills some caches on first use without locking: mail
    // templates on registration and login, and blob containers on the first
    // attachment upload. Those first requests must not run in parallel.
    WARMED_UP
        .get_or_init(|| async {
            for url in [&sandbox.source_url, &sandbox.dest_url] {
                let account = sandbox.register(url).await.expect("warm-up registration");
                let account = sandbox.open(&account).await.expect("warm-up login");
                let draft = Draft {
                    content: [
                        ("type".to_string(), Node::Number(2.into())),
                        ("name".to_string(), Node::secret("Warm-up")),
                        (
                            "secureNote".to_string(),
                            Node::Map([("type".to_string(), Node::Number(0.into()))].into()),
                        ),
                    ]
                    .into(),
                    folder_id: None,
                    collection_ids: Vec::new(),
                };
                let item = account
                    .create_item(&Scope::Personal, &draft)
                    .await
                    .expect("warm-up item");
                account
                    .upload_attachment(&Scope::Personal, &item, "warm-up.txt", b"warm-up")
                    .await
                    .expect("warm-up attachment");
            }
        })
        .await;
    Some(sandbox)
}

impl Sandbox {
    pub async fn register(&self, server: &str) -> Result<TestAccount> {
        self.register_with(
            server,
            &KdfParams::Pbkdf2 {
                iterations: 600_000,
            },
        )
        .await
    }

    pub async fn register_with(&self, server: &str, kdf: &KdfParams) -> Result<TestAccount> {
        let email = format!("test-{}@example.com", uuid::Uuid::new_v4().simple());
        provision::register(&self.http, server, &email, PASSWORD, kdf).await
    }

    /// An account without premium on the sandbox's Bitwarden server, where
    /// `sandbox/up.sh` makes every other account premium. Vaultwarden makes
    /// every account premium regardless.
    pub async fn register_free(&self, server: &str) -> Result<TestAccount> {
        let email = format!("free-{}@example.com", uuid::Uuid::new_v4().simple());
        let kdf = KdfParams::Pbkdf2 {
            iterations: 600_000,
        };
        provision::register(&self.http, server, &email, PASSWORD, &kdf).await
    }

    pub async fn open(&self, account: &TestAccount) -> Result<Account> {
        let urls = ServerUrls::from_server(&account.server)?;
        Account::open(
            self.http.clone(),
            urls,
            account.credentials.clone(),
            &account.password,
        )
        .await
    }

    /// A fresh account on the source server and one on the destination server.
    pub async fn pair(&self) -> Result<(Account, Account)> {
        let source = self.register(&self.source_url).await?;
        let dest = self.register(&self.dest_url).await?;
        Ok((self.open(&source).await?, self.open(&dest).await?))
    }
}

/// Adds `count` notes, alternately in the "Work" folder and in none, so that
/// together with the samples a first run creates enough items to import them
/// in bulk.
pub async fn seed_extra(account: &Account, count: usize) -> Result<()> {
    let work = account
        .load(&Scope::Personal)
        .await?
        .folders
        .into_iter()
        .find(|folder| folder.name == "Work")
        .map(|folder| folder.id);
    for index in 0..count {
        let draft = Draft {
            content: [
                ("type".to_string(), Node::Number(2.into())),
                (
                    "name".to_string(),
                    Node::secret(format!("Extra note {index}")),
                ),
                ("notes".to_string(), Node::secret("bulk")),
                (
                    "secureNote".to_string(),
                    Node::Map([("type".to_string(), Node::Number(0.into()))].into()),
                ),
            ]
            .into(),
            folder_id: if index % 2 == 0 { work.clone() } else { None },
            collection_ids: Vec::new(),
        };
        account.create_item(&Scope::Personal, &draft).await?;
    }
    Ok(())
}

#[derive(Default)]
pub struct Capture(pub Mutex<Vec<String>>);

impl Reporter for Capture {
    fn status(&self, verb: &str, detail: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("info: {verb} {detail}"));
    }
    fn warn(&self, message: &str) {
        self.0.lock().unwrap().push(format!("warn: {message}"));
    }
}

pub async fn run(source: &Account, dest: &Account, options: &MirrorOptions) -> Result<Outcome> {
    run_scoped(source, Scope::Personal, dest, Scope::Personal, options).await
}

/// One backup directory and key for all of a test binary's runs, since the
/// key takes a while to make.
static BACKUPS: LazyLock<(tempfile::TempDir, Keys)> = LazyLock::new(|| {
    let dir = tempfile::tempdir().expect("backup directory");
    let keys = Keys::open(dir.path(), "test backup passphrase").expect("backup key");
    (dir, keys)
});

/// Reads `account`'s vault and backs it up, as a run does before it mirrors.
pub async fn protect(account: &Account, scope: &Scope) -> Result<Protected<'static>> {
    let (dir, keys) = &*BACKUPS;
    let store = Store::new(dir.path(), &uuid::Uuid::new_v4().to_string(), keys);
    let options = BackupOptions {
        retention_days: 30,
        now: time::OffsetDateTime::now_utc(),
    };
    let (_, read) = backup(account, scope, &store, &options, &Capture::default()).await?;
    Ok(read)
}

/// Backs up both vaults and mirrors, as a run does.
pub async fn run_scoped(
    source: &Account,
    source_scope: Scope,
    dest: &Account,
    dest_scope: Scope,
    options: &MirrorOptions,
) -> Result<Outcome> {
    let source_read = protect(source, &source_scope).await?;
    let dest_read = protect(dest, &dest_scope).await?;
    run_protected(
        source,
        source_scope,
        dest,
        dest_scope,
        (&source_read, &dest_read),
        options,
    )
    .await
}

/// Mirrors from reads backed up before.
pub async fn run_protected(
    source: &Account,
    source_scope: Scope,
    dest: &Account,
    dest_scope: Scope,
    (source_read, dest_read): (&Protected<'_>, &Protected<'_>),
    options: &MirrorOptions,
) -> Result<Outcome> {
    let source = Endpoint {
        account: source,
        scope: source_scope,
    };
    let dest = Endpoint {
        account: dest,
        scope: dest_scope,
    };
    Ok(mirror::mirror(
        &source,
        &dest,
        source_read,
        dest_read,
        options,
        &Capture::default(),
    )
    .await?)
}

/// Reports what a run would do, changing nothing.
pub async fn check(source: &Account, dest: &Account, options: &MirrorOptions) -> Result<Outcome> {
    let source = Endpoint {
        account: source,
        scope: Scope::Personal,
    };
    let dest = Endpoint {
        account: dest,
        scope: Scope::Personal,
    };
    let (source_vault, dest_vault) = (
        source.account.load(&source.scope).await?,
        dest.account.load(&dest.scope).await?,
    );
    Ok(mirror::check(
        &source,
        &dest,
        &source_vault,
        &dest_vault,
        options,
        &Capture::default(),
    )
    .await?)
}

/// The source item id in `item`'s marker, if it is a copy.
pub fn copied_from(item: &Item) -> Option<String> {
    let marker = item.fields().iter().rev().find(|f| is_mirror_marker(f))?;
    let value = marker.as_map()?.get("value")?.as_str()?;
    Some(value.split_whitespace().nth(1)?.to_string())
}

pub fn by_name<'a>(vault: &'a Vault, name: &str) -> &'a Item {
    let mut matches = vault
        .items
        .iter()
        .filter(|item| item.name() == name && !item.in_trash);
    let item = matches
        .next()
        .unwrap_or_else(|| panic!("no item named {name:?}"));
    assert!(
        matches.next().is_none(),
        "more than one item named {name:?}"
    );
    item
}

pub fn folder_of(vault: &Vault, item: &Item) -> Option<String> {
    let id = item.folder_id.as_deref()?;
    vault
        .folders
        .iter()
        .find(|folder| folder.id == id)
        .map(|folder| folder.name.clone())
}

/// Creates an organization owned by `owner`, with a "Default collection" the
/// owner manages, as the API does. A self-hosted Bitwarden server creates
/// organizations only from a license, so for the sandbox's Bitwarden server
/// they are written into its database instead.
pub async fn create_organization(
    sandbox: &Sandbox,
    owner: &TestAccount,
    name: &str,
) -> Result<String> {
    match provision::create_organization(&sandbox.http, owner, name).await {
        Err(error) if format!("{error:#}").contains("Only allowed when not self hosted") => {
            seed_organization(owner, name)
        }
        result => result,
    }
}

fn seed_organization(owner: &TestAccount, name: &str) -> Result<String> {
    let keys = provision::organization_keys(owner, name)?;
    let org = uuid::Uuid::new_v4();
    let member = uuid::Uuid::new_v4();
    let collection = uuid::Uuid::new_v4();
    // Every value is generated here or is base64, so none contains a quote.
    let sql = format!(
        r#"BEGIN;
INSERT INTO "Organization" ("Id", "Name", "BillingEmail", "Plan", "PlanType", "Seats", "MaxStorageGb",
  "UsePolicies", "UseSso", "UseGroups", "UseDirectory", "UseEvents", "UseTotp", "Use2fa",
  "UseApi", "UseResetPassword", "SelfHost", "UsersGetPremium", "Enabled",
  "PublicKey", "PrivateKey", "CreationDate", "RevisionDate")
VALUES ('{org}', '{name}', '{email}', 'Free', 0, 2, 10,
  false, false, false, false, false, false, false,
  false, false, true, false, true,
  '{public_key}', '{private_key}', now(), now());
INSERT INTO "OrganizationUser" ("Id", "OrganizationId", "UserId", "Key", "Status", "Type",
  "AccessSecretsManager", "AccessPam", "CreationDate", "RevisionDate")
VALUES ('{member}', '{org}', '{user}', '{owner_key}', 2, 0, false, false, now(), now());
INSERT INTO "Collection" ("Id", "OrganizationId", "Name", "CreationDate", "RevisionDate")
VALUES ('{collection}', '{org}', '{collection_name}', now(), now());
INSERT INTO "CollectionUsers" ("CollectionId", "OrganizationUserId", "ReadOnly", "HidePasswords", "Manage")
VALUES ('{collection}', '{member}', false, false, true);
COMMIT;"#,
        email = owner.email,
        user = owner.user_id,
        public_key = keys.public_key,
        private_key = keys.encrypted_private_key,
        owner_key = keys.owner_key,
        collection_name = keys.default_collection_name,
    );
    anyhow::ensure!(
        !name.contains('\''),
        "organization names must not contain quotes"
    );
    official_sql(&sql)?;
    Ok(org.to_string())
}

/// Runs SQL in the database of the sandbox's Bitwarden server.
pub fn official_sql(sql: &str) -> Result<()> {
    let compose = concat!(env!("CARGO_MANIFEST_DIR"), "/sandbox/compose.yaml");
    let output = std::process::Command::new("docker")
        .args([
            "compose",
            "-f",
            compose,
            "--profile",
            "official",
            "exec",
            "-T",
        ])
        .args([
            "official-db",
            "psql",
            "-U",
            "bitwarden",
            "-d",
            "vault",
            "-v",
            "ON_ERROR_STOP=1",
            "-q",
        ])
        .args(["-c", sql])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "SQL in the sandbox's Bitwarden database failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
