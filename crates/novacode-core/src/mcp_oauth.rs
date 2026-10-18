//! OAuth for HTTP MCP servers (L7.4), as the MCP authorization spec describes:
//! protected-resource metadata (RFC 9728), then authorization-server metadata
//! (RFC 8414 / OIDC), then dynamic client registration (RFC 7591) unless a client id
//! is configured, then PKCE with the `resource` parameter (RFC 8707), with refresh.
//! The browser part (open a URL, catch the redirect) lives in the binary.
//!
//! Tokens live in `<home>/mcp-auth.json` (0600), keyed by server name and URL,
//! so pointing a name at another server never sends it the old token.

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use novacode_base::storage;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Refresh this long before the token runs out.
const EARLY: u64 = 60_000;

/// `oauth` in a server's config, for servers that don't allow dynamic registration.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConfig {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    /// Fixed loopback port, for clients registered with an exact redirect URI.
    #[serde(default)]
    pub callback_port: Option<u16>,
}

/// What's saved per server: enough to refresh without discovering again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub url: String,
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    pub access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
    /// Unix ms; `None` = no expiry given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn key(name: &str, url: &str) -> String {
    format!("{name} {url}")
}

pub fn store_path(home: &Path) -> PathBuf {
    home.join("mcp-auth.json")
}

fn read_all(path: &Path) -> BTreeMap<String, Saved> {
    storage::read_json(path).ok().flatten().unwrap_or_default()
}

async fn update(path: &Path, f: impl FnOnce(&mut BTreeMap<String, Saved>) + Send + 'static) -> Result<(), String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _l = storage::lock(&path).map_err(|e| e.to_string())?;
        let mut all = read_all(&path);
        f(&mut all);
        let bytes = serde_json::to_vec_pretty(&all).map_err(|e| e.to_string())?;
        storage::write_secret(&path, &bytes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn remove(home: &Path, name: &str) -> Result<bool, String> {
    let had = read_all(&store_path(home))
        .keys()
        .any(|k| k.split(' ').next() == Some(name));
    let prefix = format!("{name} ");
    update(&store_path(home), move |all| all.retain(|k, _| !k.starts_with(&prefix))).await?;
    Ok(had)
}

/// Bearer tokens for one server, refreshed when due (single-flight per session).
pub struct Auth {
    path: PathBuf,
    key: String,
    http: reqwest::Client,
    refreshing: tokio::sync::Mutex<()>,
}

impl Auth {
    pub fn new(home: &Path, name: &str, url: &str) -> Self {
        Self {
            path: store_path(home),
            key: key(name, url),
            http: novacode_ai::http::client(),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    fn saved(&self) -> Option<Saved> {
        read_all(&self.path).remove(&self.key)
    }

    pub fn logged_in(&self) -> bool {
        self.saved().is_some()
    }

    /// The token for the next request; `None` when not logged in.
    pub async fn token(&self) -> Result<Option<String>, String> {
        let Some(s) = self.saved() else { return Ok(None) };
        if s.expires_ms.is_some_and(|e| now_ms() + EARLY >= e) && s.refresh.is_some() {
            return self.refresh(&s.access).await.map(Some);
        }
        Ok(Some(s.access))
    }

    /// After a 401 for `rejected`: a new token, unless another request already got one.
    pub async fn refresh(&self, rejected: &str) -> Result<String, String> {
        let _g = self.refreshing.lock().await;
        let s = self.saved().ok_or("not logged in")?;
        if s.access != rejected {
            return Ok(s.access);
        }
        let refresh = s.refresh.clone().ok_or("the login has expired")?;
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh.clone()),
        ];
        if let Some(r) = &s.resource {
            form.push(("resource", r.clone()));
        }
        let t = token_request(
            &self.http,
            &s.token_endpoint,
            &s.client_id,
            s.client_secret.as_deref(),
            form,
        )
        .await
        .map_err(|e| format!("refreshing the login failed: {e}"))?;
        let mut next = s;
        next.access = t.access_token.clone();
        next.refresh = t.refresh_token.or(Some(refresh)); // servers may keep the old one
        next.expires_ms = t.expires_in.map(|s| now_ms() + s * 1000);
        let key = self.key.clone();
        update(&self.path, move |all| {
            all.insert(key, next);
        })
        .await?;
        Ok(t.access_token)
    }
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    secret: Option<&str>,
    mut form: Vec<(&str, String)>,
) -> Result<Tokens, String> {
    let mut req = http
        .post(endpoint)
        .timeout(TIMEOUT)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded");
    match secret {
        // shortcut: client_secret_post only; add basic when a server needs it.
        Some(s) => {
            form.push(("client_id", client_id.into()));
            form.push(("client_secret", s.into()));
        }
        None => form.push(("client_id", client_id.into())),
    }
    req = req.body(form_body(&form));
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    if let Some(e) = v["error"].as_str() {
        return Err(format!("{e}: {}", v["error_description"].as_str().unwrap_or("")));
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
    }
    serde_json::from_value(v).map_err(|e| format!("bad token response: {e}"))
}

fn form_body(form: &[(&str, String)]) -> String {
    let mut u = Url::parse("x:").expect("static url");
    u.query_pairs_mut()
        .extend_pairs(form.iter().map(|(k, v)| (*k, v.as_str())));
    u.query().unwrap_or_default().to_string()
}

/// Endpoints must be https, except loopback (for local servers and tests).
fn secure(u: &str) -> Result<Url, String> {
    let url = Url::parse(u).map_err(|e| format!("bad URL {u}: {e}"))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if url.scheme() != "https" && !local {
        return Err(format!("{u} is not https"));
    }
    Ok(url)
}

/// A login in progress: open `url`, then hand the redirect's code to [`finish`].
pub struct Pending {
    pub url: String,
    pub state: String,
    pub redirect: String,
    verifier: String,
    server_url: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
}

async fn get_json(http: &reqwest::Client, url: &Url) -> Option<Value> {
    let r = http
        .get(url.clone())
        .timeout(TIMEOUT)
        .header("accept", "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    serde_json::from_slice(&r.bytes().await.ok()?).ok()
}

/// `resource_metadata` and `scope` from a `WWW-Authenticate: Bearer …` challenge.
fn challenge(header: &str) -> (Option<String>, Option<String>) {
    let field = |name: &str| {
        let i = header.find(&format!("{name}="))? + name.len() + 1;
        let rest = &header[i..];
        let v = match rest.strip_prefix('"') {
            Some(q) => q.split('"').next()?,
            None => rest.split([',', ' ']).next()?,
        };
        (!v.is_empty()).then(|| v.to_string())
    };
    (field("resource_metadata"), field("scope"))
}

/// Discover, register if needed, and build the authorization URL.
/// `redirect` is the loopback URL the binary listens on.
pub async fn begin(server_url: &str, cfg: &OAuthConfig, redirect: &str) -> Result<Pending, String> {
    let http = novacode_ai::http::client();
    let server = Url::parse(server_url).map_err(|e| format!("bad server URL: {e}"))?;
    // 1. The server's challenge may point at its metadata.
    let probe = http
        .post(server.clone())
        .timeout(TIMEOUT)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#)
        .send()
        .await
        .map_err(|e| format!("can't reach {server_url}: {e}"))?;
    let (meta_url, challenged_scope) = probe
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(challenge)
        .unwrap_or_default();
    // 2. Protected-resource metadata: the challenge's URL, else well-known with path, then root.
    let path = server.path().trim_end_matches('/');
    let mut candidates: Vec<Url> = meta_url.iter().filter_map(|u| Url::parse(u).ok()).collect();
    for p in [
        format!("/.well-known/oauth-protected-resource{path}"),
        "/.well-known/oauth-protected-resource".into(),
    ] {
        if let Ok(u) = server.join(&p) {
            candidates.push(u);
        }
    }
    let mut resource_meta = None;
    for u in candidates {
        if let Some(v) = get_json(&http, &u).await {
            resource_meta = Some(v);
            break;
        }
    }
    let resource = match &resource_meta {
        Some(m) => {
            let r = m["resource"].as_str().unwrap_or(server_url).to_string();
            let ru = Url::parse(&r).map_err(|e| format!("bad resource {r}: {e}"))?;
            if ru.origin() != server.origin() {
                return Err(format!("protected resource {r} doesn't match the server {server_url}"));
            }
            Some(r)
        }
        None => None,
    };
    let issuer = resource_meta
        .as_ref()
        .and_then(|m| m["authorization_servers"][0].as_str().map(String::from))
        .unwrap_or_else(|| server.origin().ascii_serialization());
    // 3. Authorization-server metadata (RFC 8414, then OIDC), issuer checked.
    let iu = Url::parse(&issuer).map_err(|e| format!("bad issuer {issuer}: {e}"))?;
    let ipath = iu.path().trim_end_matches('/');
    let mut meta = None;
    for p in [
        format!("/.well-known/oauth-authorization-server{ipath}"),
        format!("/.well-known/openid-configuration{ipath}"),
        format!("{ipath}/.well-known/openid-configuration"),
    ] {
        if let Ok(u) = iu.join(&p)
            && let Some(v) = get_json(&http, &u).await
        {
            let trim = |s: &str| s.trim_end_matches('/').to_string();
            if v["issuer"].as_str().map(trim) != Some(trim(&issuer)) {
                return Err(format!(
                    "authorization server metadata names another issuer ({})",
                    v["issuer"]
                ));
            }
            meta = Some(v);
            break;
        }
    }
    let endpoint = |k: &str, def: &str| -> Result<String, String> {
        let u = meta
            .as_ref()
            .and_then(|m| m[k].as_str().map(String::from))
            .unwrap_or_else(|| iu.join(def).map(|u| u.to_string()).unwrap_or_default());
        secure(&u).map(|u| u.to_string())
    };
    let authorize = endpoint("authorization_endpoint", "/authorize")?;
    let token = endpoint("token_endpoint", "/token")?;
    if let Some(m) = &meta
        && let Some(methods) = m["code_challenge_methods_supported"].as_array()
        && !methods.iter().any(|x| x == "S256")
    {
        return Err("the authorization server doesn't support PKCE S256".into());
    }
    let scope = cfg.scope.clone().or(challenged_scope).or_else(|| {
        let s: Vec<&str> = resource_meta.as_ref()?["scopes_supported"]
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        (!s.is_empty()).then(|| s.join(" "))
    });
    // 4. Client: configured, else dynamic registration.
    let (client_id, client_secret) = match &cfg.client_id {
        Some(id) => (id.clone(), cfg.client_secret.clone()),
        None => {
            let reg = meta
                .as_ref()
                .and_then(|m| m["registration_endpoint"].as_str())
                .ok_or("the server doesn't allow dynamic client registration; set oauth.clientId in its MCP config")?;
            let mut body = serde_json::json!({
                "client_name": "novacode",
                "redirect_uris": [redirect],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
                "application_type": "native",
            });
            if let Some(s) = &scope {
                body["scope"] = s.clone().into();
            }
            let r = http
                .post(secure(reg)?)
                .timeout(TIMEOUT)
                .header("content-type", "application/json")
                .header("accept", "application/json")
                .body(body.to_string())
                .send()
                .await
                .map_err(|e| format!("client registration failed: {e}"))?;
            let status = r.status();
            let v: Value = serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or_default();
            let id = v["client_id"]
                .as_str()
                .ok_or_else(|| format!("client registration failed (HTTP {status}): {v}"))?;
            (id.to_string(), v["client_secret"].as_str().map(String::from))
        }
    };
    // 5. PKCE authorization URL.
    let (verifier, challenge) = novacode_ai::auth::pkce();
    let state = novacode_ai::auth::pkce().0;
    let mut url = Url::parse(&authorize).map_err(|e| e.to_string())?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", redirect)
            .append_pair("state", &state);
        if let Some(s) = &scope {
            q.append_pair("scope", s);
            if s.split_whitespace().any(|x| x == "offline_access") {
                q.append_pair("prompt", "consent");
            }
        }
        if let Some(r) = &resource {
            q.append_pair("resource", r);
        }
    }
    Ok(Pending {
        url: url.to_string(),
        state,
        redirect: redirect.into(),
        verifier,
        server_url: server_url.into(),
        token_endpoint: token,
        client_id,
        client_secret,
        resource,
        scope,
    })
}

/// Trade the redirect's code for tokens and save them for `name`.
pub async fn finish(home: &Path, name: &str, p: Pending, code: &str) -> Result<(), String> {
    let http = novacode_ai::http::client();
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("code_verifier", p.verifier.clone()),
        ("redirect_uri", p.redirect.clone()),
    ];
    if let Some(r) = &p.resource {
        form.push(("resource", r.clone()));
    }
    let t = token_request(&http, &p.token_endpoint, &p.client_id, p.client_secret.as_deref(), form).await?;
    let saved = Saved {
        url: p.server_url.clone(),
        token_endpoint: p.token_endpoint,
        client_id: p.client_id,
        client_secret: p.client_secret,
        resource: p.resource,
        access: t.access_token,
        refresh: t.refresh_token,
        expires_ms: t.expires_in.map(|s| now_ms() + s * 1000),
        scope: t.scope.or(p.scope),
    };
    let k = key(name, &p.server_url);
    update(&store_path(home), move |all| {
        all.insert(k, saved);
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_challenges() {
        let (m, s) = challenge(
            r#"Bearer error="invalid_token", resource_metadata="https://x/.well-known/oauth-protected-resource", scope="a b""#,
        );
        assert_eq!(m.as_deref(), Some("https://x/.well-known/oauth-protected-resource"));
        assert_eq!(s.as_deref(), Some("a b"));
        assert_eq!(challenge("Bearer realm=x"), (None, None));
        assert!(secure("http://evil.example/token").is_err());
        assert!(secure("http://127.0.0.1:9/token").is_ok());
        assert_eq!(form_body(&[("a", "x y&z".into())]), "a=x+y%26z");
    }
}
