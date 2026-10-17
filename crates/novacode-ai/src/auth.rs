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

    pub fn has(&self, provider: &str) -> bool {
        self.read()
            .map(|f| f.providers.get(provider).is_some_and(|a| !a.is_empty()))
            .unwrap_or(false)
    }

    /// A valid access token, refreshing first when needed. Not cancel-safe on
    /// purpose: callers must not drop this future mid-refresh.
    pub async fn access_token(&self, provider: &str, account: &str) -> Result<String, AuthError> {
        let cred = self
            .read()?
            .providers
            .get(provider)
            .and_then(|a| a.get(account))
            .cloned();
        let cred = cred.ok_or_else(|| AuthError::NotLoggedIn(provider.into()))?;
        if cred.expires_ms > now_ms() + REFRESH_MARGIN.as_millis() as u64 {
            return Ok(cred.access);
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
            return Ok(cred.access);
        }
        let fresh = claude::refresh(&cred.refresh).await?;
        let mut f = self.read()?;
        f.providers
            .entry(provider.into())
            .or_default()
            .insert(account.into(), fresh.clone());
        write(&self.path, &f)?;
        Ok(fresh.access)
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

    fn encode(s: &str) -> String {
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
