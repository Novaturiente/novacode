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
        Self { path: path.into() }
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

    pub async fn remove(&self, provider: &str) -> Result<bool, AuthError> {
        let _lock = self.locked().await?;
        let mut f = self.read()?;
        let had = f.providers.remove(provider).is_some();
        write(&self.path, &f)?;
        Ok(had)
    }

    /// Every stored token with a label, for masking tool output (L6.6). Read errors give none.
    pub fn secrets(&self) -> Vec<(String, String)> {
        let Ok(f) = self.read() else { return Vec::new() };
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
        let mut fresh = match provider {
            codex::PROVIDER => codex::refresh(&cred.refresh).await?,
            antigravity::PROVIDER => antigravity::refresh(&cred.refresh).await?,
            _ => claude::refresh(&cred.refresh).await?,
        };
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

/// Claude Pro/Max OAuth (manual-paste flow; works over SSH).
pub mod claude {
    use super::*;

    pub const PROVIDER: &str = "claude";
    const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
    const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
    const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
    const SCOPES: &str =
        "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

    /// URL to open; the page shows a `code#state` string to paste back.
    pub fn authorize_url(verifier: &str, challenge: &str) -> String {
        let q = [
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
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

    pub async fn exchange(pasted: &str, verifier: &str) -> Result<OAuthCredential, AuthError> {
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
            "redirect_uri": REDIRECT_URI,
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
            return Err(AuthError::Http(format!("token endpoint returned {status}: {text}")));
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

/// ChatGPT Plus/Pro (Codex) OAuth: device-code login (works over SSH), refresh,
/// and the account id from the token (L2.13).
pub mod codex {
    use super::*;

    pub const PROVIDER: &str = "codex";
    const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
    const AUTH: &str = "https://auth.openai.com";
    const DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";
    pub const VERIFY_URL: &str = "https://auth.openai.com/codex/device";

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
            return Err(AuthError::Http(format!("token endpoint returned {status}: {text}")));
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
    // Google's public desktop client for Antigravity (not a secret: installed-app OAuth).
    const CLIENT_ID: &str = "moc.tnetnocresuelgoog.sppa.pe304g4hjolotv532ercl12h2nisshmt-1950606001701";
    const CLIENT_SECRET: &str = "fADq6z4CXs8BLm1JLdL684RWF85K-XPSCOG";
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
            ("client_id", CLIENT_ID),
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
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
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
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
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
            return Err(AuthError::Http(format!("token endpoint returned {status}: {text}")));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(claude::authorize_url(&v, &c).contains("scope=org%3Acreate_api_key%20user%3Aprofile"));
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
        assert!(s.has("claude") && s.remove("claude").await.unwrap() && !s.has("claude"));
    }

    #[test]
    fn key_resolution() {
        assert_eq!(resolve_key(" sk-1 ").unwrap(), "sk-1");
        assert_eq!(resolve_key("!printf k2").unwrap(), "k2");
        assert_eq!(resolve_key("$PATH").unwrap(), std::env::var("PATH").unwrap());
        assert!(resolve_key("$NOVACODE_SURELY_UNSET_VAR").is_err());
    }
}
