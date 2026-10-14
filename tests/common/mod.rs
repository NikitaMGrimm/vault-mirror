//! Shared setup for tests that run against the sandbox servers in
//! `sandbox/compose.yaml`.
#![allow(dead_code)]

use std::{collections::HashMap, sync::Mutex};

use anyhow::Result;
use vault_mirror::{
    api::{ClientOptions, ServerUrls, build_http_client},
    crypto::KdfParams,
    fixtures,
    mirror::{Endpoint, MirrorOptions, Outcome, Reporter, mirror},
    provision::{self, TestAccount},
    vault::{Account, Draft, Item, Scope, Vault},
};

pub const PASSWORD: &str = "sandbox master password";

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
        source_url: env("VAULT_MIRROR_SANDBOX_SOURCE", "http://127.0.0.1:18081"),
        dest_url: env("VAULT_MIRROR_SANDBOX_DEST", "http://127.0.0.1:18082"),
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

/// Fills an individual vault with the sample items.
pub async fn seed(account: &Account) -> Result<()> {
    let mut folders = HashMap::new();
    for name in fixtures::FOLDERS {
        folders.insert(*name, account.create_folder(name).await?.id);
    }
    for sample in fixtures::samples() {
        let draft = Draft {
            content: sample.content,
            folder_id: sample.folder.map(|name| folders[name].clone()),
            collection_ids: Vec::new(),
        };
        let item = account.create_item(&Scope::Personal, &draft).await?;
        for (name, data) in &sample.attachments {
            account
                .upload_attachment(&Scope::Personal, &item, name, data)
                .await?;
        }
        if sample.archived {
            account
                .set_archived(std::slice::from_ref(&item.id), true)
                .await?;
        }
    }
    Ok(())
}

#[derive(Default)]
pub struct Capture(pub Mutex<Vec<String>>);

impl Reporter for Capture {
    fn info(&self, message: &str) {
        self.0.lock().unwrap().push(format!("info: {message}"));
    }
    fn warn(&self, message: &str) {
        self.0.lock().unwrap().push(format!("warn: {message}"));
    }
}

pub async fn run(source: &Account, dest: &Account, options: &MirrorOptions) -> Result<Outcome> {
    run_scoped(source, Scope::Personal, dest, Scope::Personal, options).await
}

pub async fn run_scoped(
    source: &Account,
    source_scope: Scope,
    dest: &Account,
    dest_scope: Scope,
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
    Ok(mirror(&source, &dest, options, &Capture::default()).await?)
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
