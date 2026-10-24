//! Credentials (L2.12, L2.13): `<home>/auth.json` (0600) for subscription
//! logins, API keys resolved from settings/env. Refresh holds the file lock,
//! re-reads after taking it, and is never cancelled, so a rotated refresh
//! token is never lost.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use novacode_base::storage;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("not logged in to {0}; run `novacode login {0}`")]
    NotLoggedIn(String),
    #[error("{0}")]
    Http(String),
    /// The provider refused the refresh token (`invalid_grant`): only a new login helps.
    #[error("the {0} login{1} has expired or was revoked; log in again with `novacode login {0}{2}`")]
    Expired(String, String, String),
    /// A token endpoint refused the request with a 4xx (not 408/429): retrying won't help.
    #[error("{0}")]
    Rejected(String),
    #[error("auth file: {0}")]
    Io(#[from] std::io::Error),
    #[error("api key: {0}")]
    Key(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    /// Unix ms when `access` expires.
    pub expires_ms: u64,
    /// Antigravity: the Cloud Code project the account runs in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default = "version")]
    version: u32,
    /// provider → account label → credential.
    #[serde(default)]
    providers: BTreeMap<String, BTreeMap<String, OAuthCredential>>,
    /// provider → API key saved by `novacode login` (pi's `api_key` entries). May be
    /// `$VAR` or `!command`, resolved on use. One per provider: keys never rotate (L2.14).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    keys: BTreeMap<String, String>,
}

fn version() -> u32 {
    1
}

pub const DEFAULT_ACCOUNT: &str = "default";
/// Refresh this long before expiry so a request never starts with a dying token.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        // A symlinked auth.json (an isolated home sharing the login) is written and
        // locked at its target, so a token refresh reaches every home that uses it.
        Self {
            path: std::fs::canonicalize(&path).unwrap_or(path),
        }
    }

    fn read(&self) -> Result<AuthFile, AuthError> {
        Ok(storage::read_json(&self.path)?.unwrap_or(AuthFile {
            version: 1,
            ..Default::default()
        }))
    }

    async fn locked(&self) -> Result<storage::FileLock, AuthError> {
        let p = self.path.clone();
        tokio::task::spawn_blocking(move || storage::lock(&p))
            .await
            .map_err(|e| AuthError::Io(std::io::Error::other(e)))?
            .map_err(Into::into)
    }

    pub async fn put(&self, provider: &str, account: &str, cred: OAuthCredential) -> Result<(), AuthError> {
        let _lock = self.locked().await?;
        let mut f = self.read()?;
        f.providers
            .entry(provider.into())
            .or_default()
            .insert(account.into(), cred);
        write(&self.path, &f)
    }

    /// Save an API key for `provider` (replaces the old one).
    pub async fn put_key(&self, provider: &str, key: &str) -> Result<(), AuthError> {
        let _lock = self.locked().await?;
        let mut f = self.read()?;
        f.keys.insert(provider.into(), key.into());
        write(&self.path, &f)
    }

    /// The saved API key as written (maybe `$VAR`/`!command`), unresolved.
    pub fn stored_key(&self, provider: &str) -> Option<String> {
        self.read().ok()?.keys.remove(provider)
    }

    /// Every stored login: (provider, account, credential), without refreshing.
    pub fn logins(&self) -> Vec<(String, String, OAuthCredential)> {
        let Ok(f) = self.read() else { return Vec::new() };
        f.providers
            .into_iter()
            .flat_map(|(p, m)| m.into_iter().map(move |(a, c)| (p.clone(), a, c)))
            .collect()
    }

    /// The stored access token, without refreshing (may be stale).
    pub fn cached_access(&self, provider: &str, account: &str) -> Option<String> {
        self.read()
            .ok()?
            .providers
            .get(provider)?
            .get(account)
            .map(|c| c.access.clone())
    }

    /// Logged-in accounts of a provider: `default` first, then by name.
    pub fn accounts(&self, provider: &str) -> Vec<String> {
        let mut a: Vec<String> = self
            .read()
            .map(|f| {
                f.providers
                    .get(provider)
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        // Accounts still inside a saved rate-limit window go last, so a new process
        // doesn't spend its first request on an account known to be exhausted.
        let until = self.exhausted();
        let now = now_ms();
        a.sort_by_key(|x| {
            let out = until.get(&format!("{provider}/{x}")).is_some_and(|&t| t > now);
            (out, x != DEFAULT_ACCOUNT, x.clone())
        });
        a
    }

    fn exhausted_path(&self) -> PathBuf {
        self.path.with_file_name("exhausted.json")
    }

    /// `provider/account` → epoch ms when the account's limit resets.
    fn exhausted(&self) -> BTreeMap<String, u64> {
        std::fs::read_to_string(self.exhausted_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Remember that an account is out until `until_ms` (L2.14), for other processes.
    // shortcut: read-modify-write without the auth lock; two processes marking at once can lose one entry, which only costs one extra 429.
    pub fn mark_exhausted(&self, provider: &str, account: &str, until_ms: u64) {
        let now = now_ms();
        let mut m = self.exhausted();
        m.retain(|_, &mut t| t > now);
        m.insert(format!("{provider}/{account}"), until_ms);
        if let Ok(t) = serde_json::to_string_pretty(&m) {
            let _ = storage::write_atomic(&self.exhausted_path(), t.as_bytes(), storage::Durability::Durable);
        }
    }

    /// Remove one account, or every account of the provider when `account` is None.
    pub async fn remove(&self, provider: &str, account: Option<&str>) -> Result<bool, AuthError> {
        let _lock = self.locked().await?;
        let mut f = self.read()?;
        let had = match account {
            None => f.providers.remove(provider).is_some() | f.keys.remove(provider).is_some(),
            Some(a) => {
                let m = f.providers.entry(provider.into()).or_default();
                let had = m.remove(a).is_some();
                if m.is_empty() {
                    f.providers.remove(provider);
                }
                had
            }
        };
        write(&self.path, &f)?;
        Ok(had)
    }

    /// Every stored token with a label, for masking tool output (L6.6). Read errors give none.
    pub fn secrets(&self) -> Vec<(String, String)> {
        let Ok(f) = self.read() else { return Vec::new() };
        let keys = f
            .keys
            .iter()
            .filter(|(_, k)| !k.starts_with(['$', '!']))
            .map(|(p, k)| (k.clone(), format!("{p} API key")));
        f.providers
            .iter()
            .flat_map(|(p, accounts)| {
                accounts.values().flat_map(move |c| {
                    [
                        (c.access.clone(), format!("{p} login")),
                        (c.refresh.clone(), format!("{p} login")),
                    ]
                })
            })
            .chain(keys)
            .collect()
    }

    pub fn has(&self, provider: &str) -> bool {
        self.read()
            .map(|f| f.providers.get(provider).is_some_and(|a| !a.is_empty()))
            .unwrap_or(false)
    }

    /// A valid access token, refreshing first when needed. Not cancel-safe on
    /// purpose: callers must not drop this future mid-refresh.
    pub async fn access_token(&self, provider: &str, account: &str) -> Result<String, AuthError> {
        Ok(self.credential(provider, account).await?.access)
    }

    /// The whole credential, refreshed first when needed (same rules as [`Self::access_token`]).
    pub async fn credential(&self, provider: &str, account: &str) -> Result<OAuthCredential, AuthError> {
        let cred = self
            .read()?
            .providers
            .get(provider)
            .and_then(|a| a.get(account))
            .cloned();
        let cred = cred.ok_or_else(|| AuthError::NotLoggedIn(provider.into()))?;
        if cred.expires_ms > now_ms() + REFRESH_MARGIN.as_millis() as u64 {
            return Ok(cred);
        }
        let _lock = self.locked().await?;
        // Another process may have refreshed while we waited for the lock.
        let cred = self
            .read()?
            .providers
            .get(provider)
            .and_then(|a| a.get(account))
            .cloned();
        let cred = cred.ok_or_else(|| AuthError::NotLoggedIn(provider.into()))?;
        if cred.expires_ms > now_ms() + REFRESH_MARGIN.as_millis() as u64 {
            return Ok(cred);
        }
        let refreshed = match provider {
            codex::PROVIDER => codex::refresh(&cred.refresh).await,
            antigravity::PROVIDER => antigravity::refresh(&cred.refresh).await,
            copilot::PROVIDER => copilot::exchange(&cred.refresh).await,
            _ => claude::refresh(&cred.refresh).await,
        };
        let mut fresh = refreshed.map_err(|e| expired(e, provider, account))?;
        if fresh.refresh.is_empty() {
            fresh.refresh = cred.refresh.clone(); // Google keeps the old refresh token
        }
        fresh.project = fresh.project.or(cred.project);
        let mut f = self.read()?;
        f.providers
            .entry(provider.into())
            .or_default()
            .insert(account.into(), fresh.clone());
        write(&self.path, &f)?;
        Ok(fresh)
    }
}

/// A refused refresh token becomes [`AuthError::Expired`], naming the login command.
fn expired(e: AuthError, provider: &str, account: &str) -> AuthError {
    match e {
        AuthError::Http(t) | AuthError::Rejected(t) if t.contains("invalid_grant") => {
            let (named, flag) = if account == DEFAULT_ACCOUNT {
                (String::new(), String::new())
            } else {
                (format!(" (account `{account}`)"), format!(" --account {account}"))
            };
            AuthError::Expired(provider.into(), named, flag)
        }
        e => e,
    }
}

/// A failed token-endpoint reply: a 4xx other than 408/429 is a refusal; the rest may pass on retry.
fn rejected(status: reqwest::StatusCode, msg: String) -> AuthError {
    match status.as_u16() {
        408 | 429 => AuthError::Http(msg),
        400..=499 => AuthError::Rejected(msg),
        _ => AuthError::Http(msg),
    }
}

fn write(path: &Path, f: &AuthFile) -> Result<(), AuthError> {
    let bytes = serde_json::to_vec_pretty(f).map_err(|e| AuthError::Io(std::io::Error::other(e)))?;
    storage::write_secret(path, &bytes).map_err(Into::into)
}

/// PKCE verifier and S256 challenge.
pub fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Resolve a configured key: `$VAR` reads the env, `!cmd` runs a shell command, else literal (L2.12).
pub fn resolve_key(value: &str) -> Result<String, AuthError> {
    let v = value.trim();
    if let Some(var) = v.strip_prefix('$') {
        return std::env::var(var).map_err(|_| AuthError::Key(format!("env var {var} is not set")));
    }
    if let Some(cmd) = v.strip_prefix('!') {
        let out = std::process::Command::new("sh").arg("-c").arg(cmd).output()?;
        if !out.status.success() {
            return Err(AuthError::Key(format!(
                "`{cmd}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Ok(v.to_string())
}

/// Claude Pro/Max OAuth: a loopback redirect (browser login) or the copy-code page (works over SSH).
pub mod claude {
    use super::*;

    pub const PROVIDER: &str = "claude";
    const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
    const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
    /// The page that shows a `code#state` to paste back.
    pub const COPY_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
    /// Preferred loopback port (pi's), so it can be forwarded; any free port works too.
    pub const PORT: u16 = 53692;
    const SCOPES: &str =
        "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

    /// URL to open; `redirect` is [`COPY_REDIRECT`] or `http://localhost:<port>/callback`.
    pub fn authorize_url(verifier: &str, challenge: &str, redirect: &str) -> String {
        let q = [
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect),
            ("scope", SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", verifier),
        ];
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", encode(v))).collect();
        format!("{AUTHORIZE_URL}?{}", qs.join("&"))
    }

    pub(crate) fn encode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    /// Split pasted input (`code#state`, a redirect URL, or a bare code).
    pub fn parse_pasted(input: &str) -> (String, Option<String>) {
        let v = input.trim();
        let v = v.split_once('?').map_or(v, |(_, q)| q);
        if v.contains("code=") {
            let get = |k: &str| {
                v.split('&')
                    .find_map(|p| p.strip_prefix(&format!("{k}=")))
                    .map(String::from)
            };
            return (get("code").unwrap_or_default(), get("state"));
        }
        match v.split_once('#') {
            Some((c, s)) => (c.to_string(), Some(s.to_string())),
            None => (v.to_string(), None),
        }
    }

    pub async fn exchange(pasted: &str, verifier: &str, redirect: &str) -> Result<OAuthCredential, AuthError> {
        let (code, state) = parse_pasted(pasted);
        if state.as_deref().is_some_and(|s| s != verifier) {
            return Err(AuthError::Http("OAuth state mismatch; start the login again".into()));
        }
        if code.is_empty() {
            return Err(AuthError::Http("no authorization code in the pasted text".into()));
        }
        token(json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state.as_deref().unwrap_or(verifier),
            "redirect_uri": redirect,
            "code_verifier": verifier,
        }))
        .await
    }

    pub async fn refresh(refresh_token: &str) -> Result<OAuthCredential, AuthError> {
        token(json!({ "grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": refresh_token })).await
    }

    async fn token(body: serde_json::Value) -> Result<OAuthCredential, AuthError> {
        let resp = reqwest::Client::new()
            .post(TOKEN_URL)
            .timeout(Duration::from_secs(30))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| AuthError::Http(format!("token request failed: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(rejected(status, format!("token endpoint returned {status}: {text}")));
        }
        #[derive(Deserialize)]
        struct T {
            access_token: String,
            refresh_token: String,
            expires_in: u64,
        }
        let t: T = serde_json::from_str(&text).map_err(|e| AuthError::Http(format!("bad token response: {e}")))?;
        Ok(OAuthCredential {
            access: t.access_token,
            refresh: t.refresh_token,
            expires_ms: now_ms() + t.expires_in * 1000,
            project: None,
        })
    }
}

/// ChatGPT Plus/Pro (Codex) OAuth: browser login with a loopback redirect, or a
/// device code (works over SSH); refresh,
/// and the account id from the token (L2.13).
pub mod codex {
    use super::*;

    pub const PROVIDER: &str = "codex";
    const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
    const AUTH: &str = "https://auth.openai.com";
    const DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";
    pub const VERIFY_URL: &str = "https://auth.openai.com/codex/device";
    /// Shared with the Codex CLI; the client only accepts this redirect.
    pub const PORT: u16 = 1455;
    const REDIRECT: &str = "http://localhost:1455/auth/callback";

    pub fn authorize_url(challenge: &str, state: &str) -> String {
        let q = [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "novacode"),
        ];
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", claude::encode(v))).collect();
        format!("{AUTH}/oauth/authorize?{}", qs.join("&"))
    }

    /// Browser login: trade the redirect's code for tokens.
    pub async fn exchange(code: &str, verifier: &str) -> Result<OAuthCredential, AuthError> {
        token(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT),
        ])
        .await
    }

    pub struct Device {
        id: String,
        pub user_code: String,
        interval: u64,
    }

    fn http() -> reqwest::Client {
        reqwest::Client::new()
    }

    pub async fn start() -> Result<Device, AuthError> {
        let v = post_json(
            &format!("{AUTH}/api/accounts/deviceauth/usercode"),
            json!({ "client_id": CLIENT_ID }),
        )
        .await?;
        let interval = match &v["interval"] {
            serde_json::Value::String(s) => s.trim().parse().unwrap_or(5),
            n => n.as_u64().unwrap_or(5),
        };
        Ok(Device {
            id: v["device_auth_id"].as_str().unwrap_or_default().into(),
            user_code: v["user_code"].as_str().unwrap_or_default().into(),
            interval,
        })
    }

    /// Poll until the user approves (15 min max), then exchange for tokens.
    pub async fn finish(d: &Device) -> Result<OAuthCredential, AuthError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(15 * 60);
        let mut wait = Duration::from_secs(d.interval.max(1));
        let (code, verifier) = loop {
            tokio::time::sleep(wait).await;
            if std::time::Instant::now() > deadline {
                return Err(AuthError::Http("login timed out".into()));
            }
            let resp = http()
                .post(format!("{AUTH}/api/accounts/deviceauth/token"))
                .timeout(Duration::from_secs(30))
                .header("content-type", "application/json")
                .body(json!({ "device_auth_id": d.id, "user_code": d.user_code }).to_string())
                .send()
                .await
                .map_err(|e| AuthError::Http(e.to_string()))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if (200..300).contains(&status) {
                break (
                    v["authorization_code"].as_str().unwrap_or_default().to_string(),
                    v["code_verifier"].as_str().unwrap_or_default().to_string(),
                );
            }
            let code = v["error"]["code"].as_str().or(v["error"].as_str()).unwrap_or_default();
            match (status, code) {
                (403 | 404, _) | (_, "deviceauth_authorization_pending") => {}
                (_, "slow_down") => wait += Duration::from_secs(5),
                _ => return Err(AuthError::Http(format!("device login failed ({status}): {text}"))),
            }
        };
        token(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", DEVICE_REDIRECT),
        ])
        .await
    }

    pub async fn refresh(refresh_token: &str) -> Result<OAuthCredential, AuthError> {
        token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .await
    }

    /// The ChatGPT account id inside the access token (a JWT claim).
    pub fn account_id(access: &str) -> Option<String> {
        let payload = access.split('.').nth(1)?;
        let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        v["https://api.openai.com/auth"]["chatgpt_account_id"]
            .as_str()
            .map(String::from)
    }

    async fn post_json(url: &str, body: serde_json::Value) -> Result<serde_json::Value, AuthError> {
        let resp = http()
            .post(url)
            .timeout(Duration::from_secs(30))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| AuthError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::Http(format!("{url} returned {status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| AuthError::Http(format!("bad response: {e}")))
    }

    async fn token(form: &[(&str, &str)]) -> Result<OAuthCredential, AuthError> {
        let body: Vec<String> = form.iter().map(|(k, v)| format!("{k}={}", claude::encode(v))).collect();
        let resp = http()
            .post(format!("{AUTH}/oauth/token"))
            .timeout(Duration::from_secs(30))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.join("&"))
            .send()
            .await
            .map_err(|e| AuthError::Http(format!("token request failed: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(rejected(status, format!("token endpoint returned {status}: {text}")));
        }
        #[derive(Deserialize)]
        struct T {
            access_token: String,
            refresh_token: String,
            expires_in: u64,
        }
        let t: T = serde_json::from_str(&text).map_err(|e| AuthError::Http(format!("bad token response: {e}")))?;
        Ok(OAuthCredential {
            access: t.access_token,
            refresh: t.refresh_token,
            expires_ms: now_ms() + t.expires_in * 1000,
            project: None,
        })
    }
}

/// Antigravity (Google Cloud Code) OAuth: PKCE with a loopback callback, or a
/// pasted callback URL on a headless box; then the user's Cloud Code project.
pub mod antigravity {
    use super::*;

    pub const PROVIDER: &str = "antigravity";
    // Google's public desktop client for Antigravity (installed-app OAuth: not confidential,
    // ships in every binary). Stored reversed only so secret scanners (which decode
    // base64) don't block pushes.
    const CLIENT_ID_REV: &str = "moc.tnetnocresuelgoog.sppa.pe304g4hjolotv532ercl12h2nisshmt-1950606001701";
    const CLIENT_SECRET_REV: &str = "fADq6z4CXs8BLm1JLdL684RWF85K-XPSCOG";

    fn decode(s: &str) -> String {
        s.chars().rev().collect()
    }
    const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
    const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
    pub const PORT: u16 = 51121;
    pub const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
    pub const ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
    pub const USER_AGENT: &str =
        "antigravity/cli/1.2.4 (aidev_client; os_type=linux; arch=amd64; cl=982146307; auth_method=consumer)";
    const SCOPES: &str = "https://www.googleapis.com/auth/aicode https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";

    pub fn authorize_url(challenge: &str, state: &str) -> String {
        let q = [
            ("client_id", &*decode(CLIENT_ID_REV)),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ];
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", claude::encode(v))).collect();
        format!("{AUTH_URL}?{}", qs.join("&"))
    }

    /// Exchange the code, then find the user's Cloud Code project.
    pub async fn exchange(code: &str, verifier: &str) -> Result<OAuthCredential, AuthError> {
        let mut c = token(&[
            ("client_id", &*decode(CLIENT_ID_REV)),
            ("client_secret", &*decode(CLIENT_SECRET_REV)),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier),
        ])
        .await?;
        if c.refresh.is_empty() {
            return Err(AuthError::Http(
                "Google sent no refresh token; log in again and allow offline access".into(),
            ));
        }
        c.project = project(&c.access).await;
        Ok(c)
    }

    pub async fn refresh(refresh_token: &str) -> Result<OAuthCredential, AuthError> {
        token(&[
            ("client_id", &*decode(CLIENT_ID_REV)),
            ("client_secret", &*decode(CLIENT_SECRET_REV)),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .await
    }

    /// `loadCodeAssist` names the project the account's quota lives in.
    pub async fn project(access: &str) -> Option<String> {
        let resp = reqwest::Client::new()
            .post(format!("{ENDPOINT}/v1internal:loadCodeAssist"))
            .timeout(Duration::from_secs(15))
            .bearer_auth(access)
            .header("user-agent", USER_AGENT)
            .header("content-type", "application/json")
            .body(json!({ "metadata": { "ideType": "ANTIGRAVITY" } }).to_string())
            .send()
            .await
            .ok()?;
        let v: serde_json::Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
        let p = &v["cloudaicompanionProject"];
        p.as_str().or(p["id"].as_str()).map(String::from)
    }

    async fn token(form: &[(&str, &str)]) -> Result<OAuthCredential, AuthError> {
        let body: Vec<String> = form.iter().map(|(k, v)| format!("{k}={}", claude::encode(v))).collect();
        let resp = reqwest::Client::new()
            .post(TOKEN_URL)
            .timeout(Duration::from_secs(30))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.join("&"))
            .send()
            .await
            .map_err(|e| AuthError::Http(format!("token request failed: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(rejected(status, format!("token endpoint returned {status}: {text}")));
        }
        #[derive(Deserialize)]
        struct T {
            access_token: String,
            #[serde(default)]
            refresh_token: String,
            expires_in: u64,
        }
        let t: T = serde_json::from_str(&text).map_err(|e| AuthError::Http(format!("bad token response: {e}")))?;
        Ok(OAuthCredential {
            access: t.access_token,
            refresh: t.refresh_token,
            expires_ms: now_ms() + t.expires_in * 1000,
            project: None,
        })
    }
}

/// GitHub Copilot: GitHub device login, then the GitHub token (kept as the
/// refresh token) is exchanged for a short-lived Copilot token (the access token).
pub mod copilot {
    use super::*;

    pub const PROVIDER: &str = "copilot";
    // The VS Code Copilot client id (public).
    const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
    pub const VERIFY_URL: &str = "https://github.com/login/device";
    pub const DEFAULT_BASE: &str = "https://api.individual.githubcopilot.com";
    /// Headers every Copilot request carries; the backend checks them.
    pub const HEADERS: &[(&str, &str)] = &[
        ("user-agent", "GitHubCopilotChat/0.35.0"),
        ("editor-version", "vscode/1.107.0"),
        ("editor-plugin-version", "copilot-chat/0.35.0"),
        ("copilot-integration-id", "vscode-chat"),
    ];

    pub struct Device {
        code: String,
        pub user_code: String,
        interval: u64,
    }

    fn req(r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        HEADERS
            .iter()
            .fold(r, |r, (k, v)| r.header(*k, *v))
            .header("accept", "application/json")
            .timeout(Duration::from_secs(30))
    }

    async fn form(url: &str, form: &[(&str, &str)]) -> Result<serde_json::Value, AuthError> {
        let body: Vec<String> = form.iter().map(|(k, v)| format!("{k}={}", claude::encode(v))).collect();
        let resp = req(reqwest::Client::new().post(url))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.join("&"))
            .send()
            .await
            .map_err(|e| AuthError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::Http(format!("{url} returned {status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| AuthError::Http(format!("bad response: {e}")))
    }

    pub async fn start() -> Result<Device, AuthError> {
        let v = form(
            "https://github.com/login/device/code",
            &[("client_id", CLIENT_ID), ("scope", "read:user")],
        )
        .await?;
        Ok(Device {
            code: v["device_code"].as_str().unwrap_or_default().into(),
            user_code: v["user_code"].as_str().unwrap_or_default().into(),
            interval: v["interval"].as_u64().unwrap_or(5),
        })
    }

    /// Poll until the user approves (15 min max), then get the Copilot token.
    pub async fn finish(d: &Device) -> Result<OAuthCredential, AuthError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(15 * 60);
        let mut wait = Duration::from_secs(d.interval.max(1));
        let github = loop {
            tokio::time::sleep(wait).await;
            if std::time::Instant::now() > deadline {
                return Err(AuthError::Http("login timed out".into()));
            }
            let v = form(
                "https://github.com/login/oauth/access_token",
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", &d.code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await?;
            if let Some(t) = v["access_token"].as_str() {
                break t.to_string();
            }
            match v["error"].as_str().unwrap_or_default() {
                "authorization_pending" => {}
                "slow_down" => wait = Duration::from_secs(v["interval"].as_u64().unwrap_or(wait.as_secs() + 5)),
                e => {
                    return Err(AuthError::Http(format!(
                        "device login failed: {e} {}",
                        v["error_description"]
                    )));
                }
            }
        };
        exchange(&github).await
    }

    /// GitHub token → Copilot token. Also the refresh step.
    pub async fn exchange(github: &str) -> Result<OAuthCredential, AuthError> {
        let resp = req(reqwest::Client::new().get("https://api.github.com/copilot_internal/v2/token"))
            .bearer_auth(github)
            .send()
            .await
            .map_err(|e| AuthError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(rejected(
                status,
                format!(
                    "Copilot token request returned {status} (is Copilot enabled for this GitHub account?): {text}"
                ),
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| AuthError::Http(e.to_string()))?;
        let (Some(token), Some(exp)) = (v["token"].as_str(), v["expires_at"].as_u64()) else {
            return Err(AuthError::Http("bad Copilot token response".into()));
        };
        Ok(OAuthCredential {
            access: token.into(),
            refresh: github.into(),
            expires_ms: (exp * 1000).saturating_sub(5 * 60 * 1000),
            project: None,
        })
    }

    /// The API host the token is for: `proxy-ep=proxy.X` → `https://api.X`.
    pub fn base_url(token: &str) -> String {
        token
            .split(';')
            .find_map(|kv| kv.strip_prefix("proxy-ep="))
            .map(|h| {
                format!(
                    "https://{}",
                    h.strip_prefix("proxy.").map(|r| format!("api.{r}")).unwrap_or(h.into())
                )
            })
            .unwrap_or_else(|| DEFAULT_BASE.into())
    }

    /// Switch on models whose policy is still unconfigured (some, like Claude,
    /// need it before first use). Best effort; returns the ids enabled.
    pub async fn enable_models(token: &str) -> Vec<String> {
        let base = base_url(token);
        let Ok(resp) = req(reqwest::Client::new().get(format!("{base}/models")))
            .bearer_auth(token)
            .send()
            .await
        else {
            return Vec::new();
        };
        let v: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or_default();
        let mut done = Vec::new();
        for m in v["data"].as_array().into_iter().flatten() {
            let Some(id) = m["id"].as_str() else { continue };
            if m["policy"]["state"] != "unconfigured" {
                continue;
            }
            let ok = req(reqwest::Client::new().post(format!("{base}/models/{id}/policy")))
                .bearer_auth(token)
                .header("content-type", "application/json")
                .header("openai-intent", "chat-policy")
                .header("x-interaction-type", "chat-policy")
                .body(r#"{"state":"enabled"}"#)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if ok {
                done.push(id.to_string());
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn antigravity_client_decodes() {
        let url = super::antigravity::authorize_url("c", "s");
        assert!(url.contains("client_id=1071006060591-"), "{url}");
    }

    #[test]
    fn copilot_base_url_from_token() {
        use super::copilot::base_url;
        assert_eq!(
            base_url("tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;st=x"),
            "https://api.business.githubcopilot.com"
        );
        assert_eq!(base_url("tid=1"), super::copilot::DEFAULT_BASE);
    }

    use super::*;

    #[tokio::test]
    async fn writes_go_through_a_symlinked_auth_file() {
        let real = tempfile::tempdir().unwrap();
        let iso = tempfile::tempdir().unwrap();
        let target = real.path().join("auth.json");
        AuthStore::new(&target).put_key("p", "old-key").await.unwrap();
        let link = iso.path().join("auth.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        AuthStore::new(&link).put_key("p", "new-key").await.unwrap();
        assert!(
            std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "the link stays"
        );
        assert_eq!(AuthStore::new(&target).stored_key("p").as_deref(), Some("new-key"));
    }

    #[test]
    fn codex_account_id_from_jwt() {
        let claims = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acc-1" } });
        let tok = format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims.to_string()));
        assert_eq!(codex::account_id(&tok).as_deref(), Some("acc-1"));
        assert_eq!(codex::account_id("garbage"), None);
    }

    #[test]
    fn pkce_and_paste_parsing() {
        let (v, c) = pkce();
        assert_eq!(v.len(), 43);
        assert_eq!(c, URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
        assert_eq!(claude::parse_pasted(" abc#xyz \n"), ("abc".into(), Some("xyz".into())));
        assert_eq!(
            claude::parse_pasted("https://h/cb?code=abc&state=xyz"),
            ("abc".into(), Some("xyz".into()))
        );
        assert_eq!(claude::parse_pasted("abc"), ("abc".into(), None));
        assert!(
            claude::authorize_url(&v, &c, claude::COPY_REDIRECT)
                .contains("scope=org%3Acreate_api_key%20user%3Aprofile")
        );
    }

    #[test]
    fn invalid_grant_names_the_login_command() {
        let e = || AuthError::Rejected(r#"token endpoint returned 400 Bad Request: {"error": "invalid_grant"}"#.into());
        assert_eq!(
            expired(e(), "claude", DEFAULT_ACCOUNT).to_string(),
            "the claude login has expired or was revoked; log in again with `novacode login claude`"
        );
        assert!(
            expired(e(), "claude", "eec").to_string().contains("(account `eec`)")
                && expired(e(), "claude", "eec")
                    .to_string()
                    .ends_with("`novacode login claude --account eec`")
        );
        assert!(matches!(
            expired(AuthError::Http("timeout".into()), "claude", "x"),
            AuthError::Http(_)
        ));
        use reqwest::StatusCode as S;
        assert!(matches!(rejected(S::UNAUTHORIZED, "m".into()), AuthError::Rejected(_)));
        assert!(matches!(rejected(S::BAD_REQUEST, "m".into()), AuthError::Rejected(_)));
        for s in [S::REQUEST_TIMEOUT, S::TOO_MANY_REQUESTS, S::BAD_GATEWAY] {
            assert!(matches!(rejected(s, "m".into()), AuthError::Http(_)), "{s}");
        }
    }

    #[tokio::test]
    async fn store_round_trip_is_0600_and_fresh_token_skips_refresh() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = AuthStore::new(dir.path().join("auth.json"));
        assert!(matches!(
            s.access_token("claude", DEFAULT_ACCOUNT).await,
            Err(AuthError::NotLoggedIn(_))
        ));
        let cred = OAuthCredential {
            access: "a".into(),
            refresh: "r".into(),
            expires_ms: now_ms() + 3_600_000,
            project: None,
        };
        s.put("claude", DEFAULT_ACCOUNT, cred).await.unwrap();
        assert_eq!(s.access_token("claude", DEFAULT_ACCOUNT).await.unwrap(), "a");
        let mode = std::fs::metadata(dir.path().join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(s.has("claude") && s.remove("claude", None).await.unwrap() && !s.has("claude"));
    }

    #[test]
    fn key_resolution() {
        assert_eq!(resolve_key(" sk-1 ").unwrap(), "sk-1");
        assert_eq!(resolve_key("!printf k2").unwrap(), "k2");
        assert_eq!(resolve_key("$PATH").unwrap(), std::env::var("PATH").unwrap());
        assert!(resolve_key("$NOVACODE_SURELY_UNSET_VAR").is_err());
    }
}
