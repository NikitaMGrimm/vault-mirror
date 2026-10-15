//! HTTP client for the Bitwarden server API, as implemented by Bitwarden and Vaultwarden.
//!
//! Requests and responses stay as JSON values here. The API is not a stable
//! public contract, so this layer knows endpoints and transport rules only;
//! `vault` interprets the payloads.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::RwLock;

/// Sent as `Bitwarden-Client-Version`. Servers hide newer item types from
/// clients that report an old version, so this has to track current releases.
pub const DEFAULT_CLIENT_VERSION: &str = "2026.9.0";

/// The Linux CLI device type, the client this tool stands in for.
const DEVICE_TYPE: &str = "25";

const MAX_ATTEMPTS: u32 = 5;

/// The longest a server's `Retry-After` is honoured for, per attempt.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How long one request waits out rate limits in total. Bitwarden's cloud
/// accepts about 60 new items a minute, so a large first run is mostly
/// waiting.
const RATE_LIMIT_PATIENCE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{context}: the server answered {status}: {message}")]
    Status {
        context: String,
        status: StatusCode,
        message: String,
        /// How long the server asked to wait before trying again.
        retry_after: Option<Duration>,
    },
    #[error("{context}: {}", describe_transport(cause))]
    Transport {
        context: String,
        // Deliberately not exposed as the error's source: its own message
        // includes the request URL, and download URLs carry access tokens.
        cause: reqwest::Error,
    },
    #[error("{context}: unexpected response: {detail}")]
    Unexpected { context: String, detail: String },
    #[error("invalid server URL {url:?}: {detail}")]
    InvalidUrl { url: String, detail: String },
}

impl ApiError {
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// True when the server definitely did not apply the request.
    fn not_applied(&self) -> bool {
        match self {
            Self::Status { status, .. } => *status == StatusCode::TOO_MANY_REQUESTS,
            Self::Transport { cause, .. } => cause.is_connect() && !is_certificate_error(cause),
            _ => false,
        }
    }

    /// How long to wait before the next attempt: as long as the server
    /// asked, if it did, otherwise an exponential backoff.
    fn retry_delay(&self, attempt: u32) -> Duration {
        match self {
            Self::Status {
                retry_after: Some(wait),
                ..
            } => *wait,
            _ => backoff(attempt),
        }
    }

    fn transient(&self) -> bool {
        match self {
            Self::Status { status, .. } => matches!(
                *status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ),
            Self::Transport { cause, .. } => {
                (cause.is_connect() || cause.is_timeout() || cause.is_request())
                    && !is_certificate_error(cause)
            }
            _ => false,
        }
    }
}

fn causes(error: &reqwest::Error) -> impl Iterator<Item = &(dyn std::error::Error + 'static)> {
    std::iter::successors(std::error::Error::source(error), |cause| cause.source())
}

/// A certificate the client does not trust stays untrusted, so retrying is pointless.
fn is_certificate_error(error: &reqwest::Error) -> bool {
    fn invalid(cause: &(dyn std::error::Error + 'static)) -> bool {
        if let Some(tls) = cause.downcast_ref::<rustls::Error>() {
            return matches!(tls, rustls::Error::InvalidCertificate(_));
        }
        // TLS failures travel inside I/O errors, sometimes nested, and an
        // I/O error's `source` skips over what it wraps.
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            .is_some_and(|inner| invalid(inner))
    }
    causes(error).any(invalid)
}

/// Names what went wrong without repeating the URL.
fn describe_transport(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "the request timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_body() || error.is_decode() {
        "the response could not be read"
    } else {
        "the request failed"
    };
    match causes(error).last() {
        Some(root) => format!("{kind} ({root})"),
        None => kind.to_string(),
    }
}

type Result<T> = std::result::Result<T, ApiError>;

/// Where a server's API and identity services live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrls {
    pub api: String,
    pub identity: String,
}

impl ServerUrls {
    /// Accepts what users paste as their "server": a self-hosted base URL, or
    /// the address of the Bitwarden cloud web vault.
    pub fn from_server(server: &str) -> Result<Self> {
        let invalid = |detail: &str| ApiError::InvalidUrl {
            url: server.to_string(),
            detail: detail.to_string(),
        };
        let trimmed = server.trim().trim_end_matches('/');
        let url = reqwest::Url::parse(trimmed).map_err(|err| invalid(&err.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(invalid("expected an http or https URL"));
        }
        let host = url.host_str().ok_or_else(|| invalid("missing host"))?;
        let cloud = |region: &str| Self {
            api: format!("https://api.bitwarden.{region}"),
            identity: format!("https://identity.bitwarden.{region}"),
        };
        Ok(match host {
            "bitwarden.com" | "vault.bitwarden.com" | "api.bitwarden.com" => cloud("com"),
            "bitwarden.eu" | "vault.bitwarden.eu" | "api.bitwarden.eu" => cloud("eu"),
            _ => Self {
                api: format!("{trimmed}/api"),
                identity: format!("{trimmed}/identity"),
            },
        })
    }
}

/// Whether a server address uses plain `http://` for a host beyond this
/// machine and the local network, which sends the API key and access tokens
/// across the internet unencrypted. Docker service names, private addresses
/// and Tailscale are where plain HTTP is common, so they don't count.
pub fn is_plain_http_to_remote(server: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(server.trim()) else {
        return false;
    };
    let Some(host) = url.host_str().filter(|_| url.scheme() == "http") else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let local_v4 = |ip: std::net::Ipv4Addr| {
        let tailscale = ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]);
        ip.is_loopback() || ip.is_private() || ip.is_link_local() || tailscale
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => !local_v4(ip),
        Ok(std::net::IpAddr::V6(ip)) => match ip.to_ipv4_mapped() {
            Some(ip) => !local_v4(ip),
            None => !(ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()),
        },
        Err(_) => {
            const LOCAL: [&str; 6] = [
                ".localhost",
                ".local",
                ".internal",
                ".lan",
                ".home.arpa",
                ".ts.net",
            ];
            let name = host.trim_end_matches('.').to_ascii_lowercase();
            name.contains('.') && !LOCAL.iter().any(|suffix| name.ends_with(suffix))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiCredentials {
    pub client_id: String,
    pub client_secret: String,
    /// Stable per installation. A fresh id on every run makes the server
    /// treat each run as a login from a new device.
    pub device_id: String,
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub client_version: String,
    pub timeout: Duration,
    /// Extra root certificates in PEM form, for servers behind a private CA.
    pub extra_root_certs: Vec<Vec<u8>>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            client_version: DEFAULT_CLIENT_VERSION.to_string(),
            timeout: Duration::from_secs(120),
            extra_root_certs: Vec::new(),
        }
    }
}

pub fn build_http_client(options: &ClientOptions) -> Result<reqwest::Client> {
    // Idempotent; the first caller in the process installs the provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut headers = reqwest::header::HeaderMap::new();
    let mut header = |name: &'static str, value: &str| {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    };
    header("bitwarden-client-name", "cli");
    header("bitwarden-client-version", &options.client_version);
    header("device-type", DEVICE_TYPE);
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("vault-mirror/", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .timeout(options.timeout)
        .connect_timeout(Duration::from_secs(20));
    for pem in &options.extra_root_certs {
        let transport = |source| ApiError::Transport {
            context: "loading extra root certificate".to_string(),
            cause: source,
        };
        for cert in reqwest::Certificate::from_pem_bundle(pem).map_err(transport)? {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder.build().map_err(|source| ApiError::Transport {
        context: "building HTTP client".to_string(),
        cause: source,
    })
}

/// An authenticated connection to one account on one server.
pub struct Session {
    http: reqwest::Client,
    urls: ServerUrls,
    credentials: ApiCredentials,
    token: RwLock<String>,
    /// Key derivation settings from the login response, when the server sends them.
    pub login_kdf: Option<(u64, u64, Option<u64>, Option<u64>)>,
}

/// How a failed request may be retried.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Reads and idempotent writes: retry anything transient.
    Safe,
    /// Writes that must not be applied twice: retry only when the server
    /// provably did not apply the request.
    OnlyIfNotApplied,
}

impl Session {
    pub async fn login(
        http: reqwest::Client,
        urls: ServerUrls,
        credentials: ApiCredentials,
    ) -> Result<Self> {
        let (token, login_kdf) = request_token(&http, &urls, &credentials).await?;
        Ok(Self {
            http,
            urls,
            credentials,
            token: RwLock::new(token),
            login_kdf,
        })
    }

    pub fn api_url(&self) -> &str {
        &self.urls.api
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(Method::GET, path, None, Retry::Safe).await
    }

    /// A write that is safe to repeat, such as a delete or a full replacement.
    pub async fn send_idempotent(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        self.call(method, path, body, Retry::Safe).await
    }

    /// A write that creates something, so a blind retry could duplicate it.
    pub async fn send_once(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        self.call(method, path, body, Retry::OnlyIfNotApplied).await
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        retry: Retry,
    ) -> Result<Value> {
        let url = format!("{}{}", self.urls.api, path);
        let context = format!("{method} {path}");
        let mut reauthenticated = false;
        let mut attempt = 1;
        let mut rate_limited_for = Duration::ZERO;
        loop {
            let token = self.token.read().await.clone();
            let mut request = self.http.request(method.clone(), &url).bearer_auth(&token);
            if let Some(body) = body {
                request = request.json(body);
            }
            let secrets = [self.credentials.client_secret.as_str(), token.as_str()];
            let error = match send(request, &context, &secrets).await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if error.status() == Some(StatusCode::UNAUTHORIZED) && !reauthenticated {
                // Access tokens are short-lived and a long attachment transfer can outlive one.
                reauthenticated = true;
                let (fresh, _) = request_token(&self.http, &self.urls, &self.credentials).await?;
                *self.token.write().await = fresh;
                continue;
            }
            let retryable = match retry {
                Retry::Safe => error.transient(),
                Retry::OnlyIfNotApplied => error.not_applied(),
            };
            let delay = match retryable {
                true => next_attempt(&error, attempt, &mut rate_limited_for),
                false => None,
            };
            let Some(delay) = delay else {
                return Err(error);
            };
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// Downloads a file from a URL the server handed out. Those URLs carry
    /// their own authorization and may point at separate storage.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let context = "downloading attachment".to_string();
        let mut attempt = 1;
        let mut rate_limited_for = Duration::ZERO;
        loop {
            let error = match self.download_once(url, &context).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) if error.transient() => error,
                Err(error) => return Err(error),
            };
            let Some(delay) = next_attempt(&error, attempt, &mut rate_limited_for) else {
                return Err(error);
            };
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    async fn download_once(&self, url: &str, context: &str) -> Result<Vec<u8>> {
        let transport = |source| ApiError::Transport {
            context: context.to_string(),
            cause: source,
        };
        let response = self.http.get(url).send().await.map_err(transport)?;
        if !response.status().is_success() {
            return Err(status_error(context, response, &self.secrets()).await);
        }
        Ok(response.bytes().await.map_err(transport)?.to_vec())
    }

    /// Uploads an encrypted attachment through the API server itself.
    pub async fn upload_direct(&self, path: &str, file_name: &str, blob: Vec<u8>) -> Result<()> {
        let context = "uploading attachment";
        let part = reqwest::multipart::Part::bytes(blob)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")
            .map_err(|source| transport(context, source))?;
        let form = reqwest::multipart::Form::new().part("data", part);
        let token = self.token.read().await.clone();
        let request = self
            .http
            .post(format!("{}{}", self.urls.api, path))
            .bearer_auth(&token)
            .multipart(form);
        let secrets = [self.credentials.client_secret.as_str(), token.as_str()];
        expect_success(request, context, &secrets).await
    }

    /// Uploads an encrypted attachment straight to Azure blob storage, which
    /// the Bitwarden cloud uses. `url` is the signed location the server issued.
    pub async fn upload_azure(&self, url: &str, blob: Vec<u8>) -> Result<()> {
        let context = "uploading attachment to blob storage";
        let mut request = self
            .http
            .put(url)
            .header("x-ms-blob-type", "BlockBlob")
            .header(
                "x-ms-date",
                httpdate::fmt_http_date(std::time::SystemTime::now()),
            );
        // The signed URL names the storage API version it was issued for.
        let version = reqwest::Url::parse(url).ok().and_then(|parsed| {
            parsed
                .query_pairs()
                .find_map(|(key, value)| (key == "sv").then(|| value.into_owned()))
        });
        if let Some(version) = version {
            request = request.header("x-ms-version", version);
        }
        expect_success(request.body(blob), context, &self.secrets()).await
    }

    /// What a server must not get to echo into an error message.
    fn secrets(&self) -> [&str; 1] {
        [self.credentials.client_secret.as_str()]
    }
}

fn transport(context: &str, source: reqwest::Error) -> ApiError {
    ApiError::Transport {
        context: context.to_string(),
        cause: source,
    }
}

async fn expect_success(
    request: reqwest::RequestBuilder,
    context: &str,
    secrets: &[&str],
) -> Result<()> {
    let response = request
        .send()
        .await
        .map_err(|source| transport(context, source))?;
    if response.status().is_success() {
        return Ok(());
    }
    Err(status_error(context, response, secrets).await)
}

/// `secrets` are values sent with the request, removed from the server's
/// message in case it repeats them.
async fn status_error(context: &str, response: reqwest::Response, secrets: &[&str]) -> ApiError {
    let status = response.status();
    let retry_after = retry_after(response.headers(), std::time::SystemTime::now());
    ApiError::Status {
        context: context.to_string(),
        status,
        message: error_message(&response.text().await.unwrap_or_default(), secrets),
        retry_after,
    }
}

/// Reads `Retry-After`, which is either a number of seconds or a date.
fn retry_after(
    headers: &reqwest::header::HeaderMap,
    now: std::time::SystemTime,
) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let wait = match value.parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        Err(_) => httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .unwrap_or(Duration::ZERO),
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

/// The wait before the next attempt at a request that failed with a
/// retryable error, or `None` to give up. A rate limit is the server asking
/// to slow down, not a failure, so it is waited out for longer than other
/// errors are retried.
fn next_attempt(
    error: &ApiError,
    attempt: u32,
    rate_limited_for: &mut Duration,
) -> Option<Duration> {
    let delay = error.retry_delay(attempt);
    if error.status() == Some(StatusCode::TOO_MANY_REQUESTS) {
        let delay = delay.max(Duration::from_secs(1));
        *rate_limited_for += delay;
        return (*rate_limited_for <= RATE_LIMIT_PATIENCE).then_some(delay);
    }
    (attempt < MAX_ATTEMPTS).then_some(delay)
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.saturating_sub(1)).min(16))
}

async fn send(request: reqwest::RequestBuilder, context: &str, secrets: &[&str]) -> Result<Value> {
    let response = request.send().await.map_err(|source| ApiError::Transport {
        context: context.to_string(),
        cause: source,
    })?;
    if !response.status().is_success() {
        return Err(status_error(context, response, secrets).await);
    }
    let text = response
        .text()
        .await
        .map_err(|source| ApiError::Transport {
            context: context.to_string(),
            cause: source,
        })?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|err| ApiError::Unexpected {
        context: context.to_string(),
        detail: format!("response is not JSON ({err})"),
    })
}

/// Extracts the human-readable part of an error body without echoing
/// arbitrary server output, which could include request data.
fn error_message(body: &str, secrets: &[&str]) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let text = parsed
        .as_ref()
        .and_then(|value| {
            ["message", "Message", "error_description", "error"]
                .iter()
                .find_map(|key| value.get(key).and_then(Value::as_str))
        })
        .unwrap_or("no error message");
    // Bitwarden's server answers invalid requests with a generic message and
    // the reasons in a map from field to messages.
    let reasons: Vec<String> = parsed
        .as_ref()
        .and_then(|value| value.get("validationErrors"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .flat_map(|(field, messages)| {
            messages
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(move |message| match field.as_str() {
                    "" => message.to_string(),
                    field => format!("{field}: {message}"),
                })
        })
        .collect();
    let mut text = match reasons.is_empty() {
        true => text.to_string(),
        false => format!("{text} {}", reasons.join(" ")),
    };
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    let text = redact_urls(&text);
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(300).collect()
}

/// Replaces every http and https URL. Servers quote the URL of a failed
/// request, and attachment URLs carry access tokens.
fn redact_urls(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    while let Some(start) = ["http://", "https://"]
        .iter()
        .filter_map(|scheme| lower[done..].find(scheme))
        .min()
        .map(|offset| done + offset)
    {
        let rest = &text[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(rest.len());
        // Punctuation that ends the sentence is not part of the URL.
        let url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        out.push_str(&text[done..start]);
        out.push_str("[URL]");
        done = start + url.len();
    }
    out.push_str(&text[done..]);
    out
}

type LoginKdf = Option<(u64, u64, Option<u64>, Option<u64>)>;

async fn request_token(
    http: &reqwest::Client,
    urls: &ServerUrls,
    credentials: &ApiCredentials,
) -> Result<(String, LoginKdf)> {
    let context = "logging in with the API key";
    let form = [
        ("grant_type", "client_credentials"),
        ("scope", "api"),
        ("client_id", credentials.client_id.as_str()),
        ("client_secret", credentials.client_secret.as_str()),
        ("deviceType", DEVICE_TYPE),
        ("deviceIdentifier", credentials.device_id.as_str()),
        ("deviceName", "vault-mirror"),
    ];
    let mut attempt = 1;
    let mut rate_limited_for = Duration::ZERO;
    let value = loop {
        let request = http
            .post(format!("{}/connect/token", urls.identity))
            .form(&form);
        let error = match send(request, context, &[&credentials.client_secret]).await {
            Ok(value) => break value,
            // A login request changes nothing, so every transient failure is retryable.
            Err(error) if error.transient() => error,
            Err(error) => return Err(error),
        };
        let Some(delay) = next_attempt(&error, attempt, &mut rate_limited_for) else {
            return Err(error);
        };
        tokio::time::sleep(delay).await;
        attempt += 1;
    };
    let token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Unexpected {
            context: context.to_string(),
            detail: "no access token in the login response".to_string(),
        })?
        .to_string();
    Ok((token, login_kdf(&value)))
}

fn login_kdf(value: &Value) -> LoginKdf {
    // Servers have spelled these fields in both PascalCase and camelCase.
    let field = |name: &str| {
        value.as_object()?.iter().find_map(|(key, value)| {
            key.eq_ignore_ascii_case(name)
                .then(|| value.as_u64())
                .flatten()
        })
    };
    Some((
        field("Kdf")?,
        field("KdfIterations")?,
        field("KdfMemory"),
        field("KdfParallelism"),
    ))
}

/// Asks the server for an account's key derivation settings. Used when the
/// login response does not carry them.
pub async fn prelogin(http: &reqwest::Client, urls: &ServerUrls, email: &str) -> Result<LoginKdf> {
    let request = http
        .post(format!("{}/accounts/prelogin", urls.identity))
        .json(&json!({ "email": email }));
    let value = send(request, "reading key derivation settings", &[]).await?;
    Ok(login_kdf(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_servers_use_path_prefixes() {
        let urls = ServerUrls::from_server("https://vault.example.com/").unwrap();
        assert_eq!(urls.api, "https://vault.example.com/api");
        assert_eq!(urls.identity, "https://vault.example.com/identity");
        let nested = ServerUrls::from_server("http://127.0.0.1:8080/bw").unwrap();
        assert_eq!(nested.api, "http://127.0.0.1:8080/bw/api");
    }

    #[test]
    fn the_bitwarden_cloud_uses_dedicated_hosts() {
        let us = ServerUrls::from_server("https://vault.bitwarden.com").unwrap();
        assert_eq!(us.api, "https://api.bitwarden.com");
        assert_eq!(us.identity, "https://identity.bitwarden.com");
        let eu = ServerUrls::from_server("https://vault.bitwarden.eu/").unwrap();
        assert_eq!(eu.api, "https://api.bitwarden.eu");
        assert_eq!(eu.identity, "https://identity.bitwarden.eu");
    }

    #[test]
    fn only_plain_http_beyond_the_local_network_counts_as_remote() {
        for remote in [
            "http://vault.example.com",
            "http://vault.example.com:8080/bw",
            "http://203.0.113.7",
            "http://[2001:db8::1]",
        ] {
            assert!(is_plain_http_to_remote(remote), "{remote}");
        }
        for local in [
            "https://vault.example.com",
            "http://localhost:8080",
            "http://vaultwarden",
            "http://127.0.0.1:8080",
            "http://192.168.1.20",
            "http://10.0.0.5",
            "http://172.20.0.3",
            "http://100.101.102.103",
            "http://[::1]:8080",
            "http://[fd00::5]",
            "http://[::ffff:127.0.0.1]",
            "http://vault.lan",
            "http://nas.local",
            "http://vault.home.arpa",
            "http://vault.tailnet-1234.ts.net",
        ] {
            assert!(!is_plain_http_to_remote(local), "{local}");
        }
    }

    #[test]
    fn malformed_server_urls_are_rejected() {
        assert!(ServerUrls::from_server("vault.example.com").is_err());
        assert!(ServerUrls::from_server("ftp://vault.example.com").is_err());
        assert!(ServerUrls::from_server("").is_err());
    }

    #[test]
    fn error_messages_are_reduced_to_the_server_message() {
        assert_eq!(
            error_message(r#"{"message":"Invalid  item","x":"secret"}"#, &[]),
            "Invalid item"
        );
        assert_eq!(
            error_message(
                r#"{"error":"invalid_client","error_description":"Bad\nkey"}"#,
                &[]
            ),
            "Bad key"
        );
        assert_eq!(
            error_message(
                r#"{"message":"The model state is invalid.","validationErrors":{"Cipher.Name":["Too long."],"":["Needs a collection."]}}"#,
                &[]
            ),
            "The model state is invalid. Cipher.Name: Too long. Needs a collection."
        );
        assert_eq!(
            error_message("<html>secret dump</html>", &[]),
            "no error message"
        );
    }

    #[test]
    fn error_messages_leave_out_urls() {
        let body = r#"{"message":"Blob https://store.example/att/1?sv=2024&sig=TOKEN%3D not found.","validationErrors":{"Url":["Use HTTP://a.example/x?token=T (or 'https://b.example')."]}}"#;
        assert_eq!(
            error_message(body, &[]),
            "Blob [URL] not found. Url: Use [URL] (or '[URL]')."
        );
        assert_eq!(
            error_message(r#"{"message":"see https://"}"#, &[]),
            "see [URL]"
        );
    }

    #[test]
    fn error_messages_leave_out_what_was_sent() {
        let body = r#"{"error":"invalid_client","error_description":"Unknown secret Sx9-k2 for user.1 (Sx9-k2)"}"#;
        assert_eq!(
            error_message(body, &["Sx9-k2", ""]),
            "Unknown secret [redacted] for user.1 ([redacted])"
        );
    }

    #[test]
    fn login_kdf_is_read_in_either_casing() {
        let pascal = json!({"Kdf": 1, "KdfIterations": 3, "KdfMemory": 64, "KdfParallelism": 4});
        assert_eq!(login_kdf(&pascal), Some((1, 3, Some(64), Some(4))));
        let camel = json!({"kdf": 0, "kdfIterations": 600000, "kdfMemory": null});
        assert_eq!(login_kdf(&camel), Some((0, 600000, None, None)));
        assert_eq!(login_kdf(&json!({"access_token": "x"})), None);
    }

    #[test]
    fn rate_limits_are_waited_out_longer_than_other_errors() {
        let error = |status, retry_after| ApiError::Status {
            context: String::new(),
            status,
            message: String::new(),
            retry_after,
        };
        let mut waited = Duration::ZERO;
        let unavailable = error(StatusCode::SERVICE_UNAVAILABLE, None);
        assert!(next_attempt(&unavailable, MAX_ATTEMPTS - 1, &mut waited).is_some());
        assert!(next_attempt(&unavailable, MAX_ATTEMPTS, &mut waited).is_none());

        let limited = error(StatusCode::TOO_MANY_REQUESTS, Some(MAX_RETRY_AFTER));
        let mut attempts = 0;
        while next_attempt(&limited, attempts + 1, &mut waited).is_some() {
            attempts += 1;
        }
        assert_eq!(
            attempts as u64,
            RATE_LIMIT_PATIENCE.as_secs() / MAX_RETRY_AFTER.as_secs()
        );
    }

    #[test]
    fn retry_after_is_read_as_seconds_or_a_date_and_capped() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let read = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            retry_after(&headers, now)
        };
        assert_eq!(read("7"), Some(Duration::from_secs(7)));
        assert_eq!(read("3600"), Some(MAX_RETRY_AFTER));
        let in_ten_seconds = httpdate::fmt_http_date(now + Duration::from_secs(10));
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, in_ten_seconds.parse().unwrap());
        assert_eq!(retry_after(&headers, now), Some(Duration::from_secs(10)));
        assert_eq!(read("Thu, 01 Jan 1970 00:00:00 GMT"), Some(Duration::ZERO));
        assert_eq!(read("soon"), None);
        assert_eq!(retry_after(&HeaderMap::new(), now), None);
    }

    #[test]
    fn a_requested_wait_replaces_the_backoff() {
        let error = |retry_after| ApiError::Status {
            context: "test".into(),
            status: StatusCode::TOO_MANY_REQUESTS,
            message: String::new(),
            retry_after,
        };
        assert_eq!(error(None).retry_delay(3), backoff(3));
        assert_eq!(
            error(Some(Duration::from_secs(20))).retry_delay(1),
            Duration::from_secs(20)
        );
    }
}
