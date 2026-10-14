//! Creates throwaway accounts and organizations on a test server.
//!
//! The integration suite and `examples/sandbox.rs` use this to get
//! API credentials without a human or the official CLI. It registers accounts
//! the way a real client does, so it works against any server that has open
//! registration and no email verification, which is how the sandbox servers
//! are configured. It is not meant for production servers.

use anyhow::{Context, Result, anyhow};
use reqwest::header::ACCEPT;
use serde_json::{Value, json};

use vault_mirror::{
    api::{ApiCredentials, ServerUrls},
    crypto::{self, KdfParams, MasterSecret, SymKey},
};

#[derive(Debug, Clone)]
pub struct TestAccount {
    pub server: String,
    pub email: String,
    pub password: String,
    pub user_id: String,
    pub credentials: ApiCredentials,
    access_token: String,
    public_key: String,
}

pub async fn register(
    http: &reqwest::Client,
    server: &str,
    email: &str,
    password: &str,
    kdf: &KdfParams,
) -> Result<TestAccount> {
    let urls = ServerUrls::from_server(server)?;
    let master = MasterSecret::derive(password, email, kdf)?;
    let keys = master.new_account_keys()?;
    let hash = master.server_hash(password);
    let kdf_json = match *kdf {
        KdfParams::Pbkdf2 { iterations } => json!({ "kdfType": 0, "iterations": iterations }),
        KdfParams::Argon2id {
            iterations,
            memory_mib,
            parallelism,
        } => {
            json!({ "kdfType": 1, "iterations": iterations, "memory": memory_mib, "parallelism": parallelism })
        }
    };
    let salt = email.trim().to_lowercase();

    let token: Value = post(
        http.post(format!(
            "{}/accounts/register/send-verification-email",
            urls.identity
        ))
        .header(ACCEPT, "application/json")
        .json(&json!({ "email": email, "name": "Sandbox" })),
        "requesting a registration token",
    )
    .await?;
    let token = token.as_str().ok_or_else(|| {
        anyhow!("the server wants email verification; registration cannot be automated")
    })?;

    post(
        http.post(format!("{}/accounts/register/finish", urls.identity))
            .json(&json!({
                "email": email,
                "emailVerificationToken": token,
                "masterPasswordHint": null,
                "masterPasswordAuthentication": {
                    "kdf": kdf_json,
                    "salt": salt,
                    "masterPasswordAuthenticationHash": hash,
                },
                "masterPasswordUnlock": {
                    "kdf": kdf_json,
                    "salt": salt,
                    "masterKeyWrappedUserKey": keys.wrapped_user_key,
                },
                "userAsymmetricKeys": {
                    "publicKey": keys.public_key,
                    "encryptedPrivateKey": keys.wrapped_private_key,
                },
            })),
        "finishing registration",
    )
    .await?;

    let device_id = uuid::Uuid::new_v4().to_string();
    let login = post(
        http.post(format!("{}/connect/token", urls.identity))
            .form(&[
                ("grant_type", "password"),
                ("username", email),
                ("password", hash.as_str()),
                ("scope", "api offline_access"),
                ("client_id", "cli"),
                ("deviceType", "25"),
                ("deviceIdentifier", device_id.as_str()),
                ("deviceName", "vault-mirror-provision"),
            ]),
        "logging in with the master password",
    )
    .await?;
    let access_token = text(&login, "access_token")?;

    let profile = get(http, &urls, &access_token, "/accounts/profile").await?;
    let user_id = text(&profile, "id")?;
    let api_key = post(
        http.post(format!("{}/accounts/api-key", urls.api))
            .bearer_auth(&access_token)
            .json(&json!({ "masterPasswordHash": hash })),
        "reading the API key",
    )
    .await?;

    Ok(TestAccount {
        server: server.to_string(),
        email: email.to_string(),
        password: password.to_string(),
        credentials: ApiCredentials {
            client_id: format!("user.{user_id}"),
            client_secret: text(&api_key, "apiKey")?,
            device_id,
        },
        user_id,
        access_token,
        public_key: keys.public_key,
    })
}

/// Creates an organization owned by `account` and returns its id.
/// The key material of a new organization, as a client generates it.
pub struct OrganizationKeys {
    /// The organization key, encrypted for its owner.
    pub owner_key: String,
    pub public_key: String,
    pub encrypted_private_key: String,
    /// "Default collection", encrypted with the organization key.
    pub default_collection_name: String,
}

pub fn organization_keys(account: &TestAccount, name: &str) -> Result<OrganizationKeys> {
    let org_key = SymKey::generate();
    // The organization gets its own RSA key pair; only its shape matters to
    // the server, so one derived from a throwaway secret is enough here.
    let org_pair = MasterSecret::derive(
        "organization",
        name,
        &KdfParams::Pbkdf2 { iterations: 5000 },
    )?
    .new_account_keys()?;
    Ok(OrganizationKeys {
        owner_key: crypto::share_key_with(&account.public_key, &org_key)?,
        public_key: org_pair.public_key,
        encrypted_private_key: org_key.encrypt_str("unused private key")?,
        default_collection_name: org_key.encrypt_str("Default collection")?,
    })
}

pub async fn create_organization(
    http: &reqwest::Client,
    account: &TestAccount,
    name: &str,
) -> Result<String> {
    let urls = ServerUrls::from_server(&account.server)?;
    let keys = organization_keys(account, name)?;
    let created = post(
        http.post(format!("{}/organizations", urls.api))
            .bearer_auth(&account.access_token)
            .json(&json!({
                "name": name,
                "billingEmail": account.email,
                "collectionName": keys.default_collection_name,
                "key": keys.owner_key,
                "keys": {
                    "publicKey": keys.public_key,
                    "encryptedPrivateKey": keys.encrypted_private_key,
                },
                "planType": 0,
            })),
        "creating an organization",
    )
    .await?;
    text(&created, "id")
}

async fn get(http: &reqwest::Client, urls: &ServerUrls, token: &str, path: &str) -> Result<Value> {
    post(
        http.get(format!("{}{}", urls.api, path)).bearer_auth(token),
        path,
    )
    .await
}

async fn post(request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
    let response = request.send().await.with_context(|| what.to_string())?;
    let status = response.status();
    let body = response.text().await.with_context(|| what.to_string())?;
    if !status.is_success() {
        let detail: String = body.chars().take(300).collect();
        return Err(anyhow!("{what}: the server answered {status}: {detail}"));
    }
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&body).with_context(|| format!("{what}: response is not JSON"))
}

fn text(value: &Value, name: &str) -> Result<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("response has no {name}"))
}
