//! Routes (L2.11): `provider/model[:level]` → profile + model metadata +
//! a ready adapter. The one place that knows how a profile becomes an adapter.

use crate::adapter::Adapter;
use crate::anthropic::{self, Anthropic, OAuthProfile};
use crate::auth::{self, AuthError, AuthStore, DEFAULT_ACCOUNT};
use crate::catalog::{self, Api, Catalog, ModelInfo, Profile};
use crate::gemini;
use crate::openai_chat::{Compat, OpenAiChat};
use crate::openai_responses;
use crate::trace::Trace;
use novacode_base::settings::Settings;
use novacode_types::ThinkingLevel;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("{0}")]
    Setup(String),
}

fn setup(msg: impl Into<String>) -> ConnectError {
    ConnectError::Setup(msg.into())
}

/// A resolved model choice.
#[derive(Debug, Clone)]
pub struct Route {
    pub provider: String,
    /// Alias already resolved (L2.27): fixed for the session.
    pub model: String,
    /// The `:level` suffix, if one was given.
    pub level: Option<ThinkingLevel>,
    pub info: ModelInfo,
    pub api: Api,
}

impl Route {
    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// The nearest thinking level this model supports (L2.16).
    pub fn clamp(&self, level: ThinkingLevel) -> ThinkingLevel {
        match self.api {
            Api::Anthropic => anthropic::clamp_thinking(&self.model, level),
            _ => catalog::clamp_thinking(&self.info, level),
        }
    }

    /// Whether a thinking change keeps the cached prefix (L1.7, L2.16).
    pub fn thinking_change_keeps_cache(&self) -> bool {
        self.api == Api::Anthropic && anthropic::thinking_style(&self.model) == anthropic::ThinkingStyle::Managed
    }
}

/// `provider/model:level` → (`provider/model`, level). A suffix that isn't a level stays part of the id.
pub fn split_level(spec: &str) -> (&str, Option<ThinkingLevel>) {
    match spec.rsplit_once(':') {
        Some((m, l)) if ThinkingLevel::parse(l).is_some() => (m, ThinkingLevel::parse(l)),
        _ => (spec, None),
    }
}

/// Profiles, model metadata and credentials, loaded once per process.
pub struct Providers {
    pub profiles: BTreeMap<String, Profile>,
    pub catalog: Catalog,
    pub auth: AuthStore,
    /// `--api-key` for one provider, ahead of every other source (pi's runtime override).
    pub cli_key: Option<(String, String)>,
}

impl Providers {
    pub fn load(home: &Path, settings: &Settings) -> Self {
        Self {
            profiles: catalog::profiles(settings.get::<Value>("providers").as_ref()),
            catalog: Catalog::load(&cache_path(home), settings.get::<Value>("models").as_ref()),
            auth: AuthStore::new(home.join("auth.json")),
            cli_key: None,
        }
    }

    pub fn profile(&self, id: &str) -> Result<&Profile, ConnectError> {
        self.profiles.get(id).ok_or_else(|| {
            setup(format!(
                "unknown provider `{id}`; known: {}",
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })
    }

    /// Has a login, a configured key, a key in the env, or needs none.
    pub fn connected(&self, p: &Profile) -> bool {
        if p.oauth() {
            return self.auth.has(&p.id);
        }
        self.key_source(p).is_some()
    }

    /// Parse and resolve a model spec. Aliases resolve against the catalog.
    pub fn resolve(&self, spec: &str) -> Result<Route, ConnectError> {
        let (spec, level) = split_level(spec.trim());
        let (provider, model) = spec
            .split_once('/')
            .ok_or_else(|| setup(format!("`{spec}`: use provider/model, e.g. claude/claude-opus-5-5")))?;
        let p = self.profile(provider)?;
        let model = self
            .catalog
            .resolve_alias(p.catalog_id(), model, None)
            .unwrap_or_else(|| model.to_string());
        Ok(Route {
            provider: provider.into(),
            info: self.catalog.info(p.catalog_id(), &model),
            model,
            level,
            api: p.api,
        })
    }

    /// The adapter for a provider, plus the API key it uses (for masking, L6.6).
    /// The adapter for a provider. Subscription logins get a live adapter that
    /// follows token refreshes and rotates between the provider's logged-in
    /// accounts when one is exhausted (L2.14). Returns the API key used, for masking.
    pub async fn adapter(
        self: &Arc<Self>,
        provider: &str,
        trace: Option<Trace>,
    ) -> Result<(Arc<dyn Adapter>, Option<String>), ConnectError> {
        let p = self.profile(provider)?;
        if !p.oauth() {
            return self.adapter_for(provider, DEFAULT_ACCOUNT, trace).await;
        }
        let accounts = self.auth.accounts(provider);
        let first = accounts.first().map(String::as_str).unwrap_or(DEFAULT_ACCOUNT);
        // Build the first account now, so a missing or broken login shows at connect time.
        let (a, _) = self.adapter_for(provider, first, trace.clone()).await?;
        let live = crate::rotate::Live::new(self.clone(), provider, accounts, trace, a);
        Ok((Arc::new(live), None))
    }

    /// The adapter for one account, with the credential as it is right now.
    pub async fn adapter_for(
        &self,
        provider: &str,
        account: &str,
        trace: Option<Trace>,
    ) -> Result<(Arc<dyn Adapter>, Option<String>), ConnectError> {
        let p = self.profile(provider)?;
        let key = if p.oauth() { None } else { self.key(p)? };
        let adapter: Arc<dyn Adapter> = match p.api {
            Api::Anthropic => {
                let auth = if p.oauth() {
                    let d = OAuthProfile::default();
                    let get = |k: &str, def: String| p.extra_str(k).unwrap_or(def);
                    anthropic::Auth::OAuth {
                        access_token: self.auth.access_token(&p.id, account).await?,
                        profile: OAuthProfile {
                            user_agent: get("userAgent", d.user_agent),
                            betas: get("betas", d.betas),
                            billing_header: get("billingHeader", d.billing_header),
                            identity: get("identity", d.identity),
                        },
                    }
                } else {
                    anthropic::Auth::ApiKey(key.clone().ok_or_else(|| setup(format!("{} needs an API key", p.id)))?)
                };
                let mut a = Anthropic::new(auth, trace);
                if !p.base_url.is_empty() {
                    a = a.with_base_url(p.base_url.clone());
                }
                if p.catalog_id() != anthropic::PROVIDER {
                    a = a.with_provider(p.id.clone());
                }
                Arc::new(a)
            }
            Api::OpenAiChat => {
                let raw = if p.compat.is_null() {
                    Value::Object(Default::default())
                } else {
                    p.compat.clone()
                };
                let compat: Compat =
                    serde_json::from_value(raw).map_err(|e| setup(format!("providers.{}.compat: {e}", p.id)))?;
                Arc::new(
                    OpenAiChat::new(&p.id, &p.base_url, key.clone(), compat, trace).with_headers(p.headers.clone()),
                )
            }
            Api::OpenAiResponses => {
                let auth = if p.oauth() {
                    openai_responses::Auth::Codex {
                        access_token: self.auth.access_token(&p.id, account).await?,
                        originator: p.extra_str("originator").unwrap_or_else(|| "novacode".into()),
                    }
                } else {
                    openai_responses::Auth::ApiKey(
                        key.clone().ok_or_else(|| setup(format!("{} needs an API key", p.id)))?,
                    )
                };
                let raw = if p.compat.is_null() {
                    Value::Object(Default::default())
                } else {
                    p.compat.clone()
                };
                let compat =
                    serde_json::from_value(raw).map_err(|e| setup(format!("providers.{}.compat: {e}", p.id)))?;
                Arc::new(
                    openai_responses::OpenAiResponses::new(&p.id, &p.base_url, auth, compat, trace)
                        .with_headers(p.headers.clone()),
                )
            }
            Api::Gemini => {
                let auth = if p.oauth() {
                    let c = self.auth.credential(&p.id, account).await?;
                    let project = match c.project {
                        Some(pr) => pr,
                        None => crate::auth::antigravity::project(&c.access)
                            .await
                            .ok_or_else(|| setup("could not find your Cloud Code project; log in again"))?,
                    };
                    gemini::Auth::Antigravity {
                        access_token: c.access,
                        project,
                        user_agent: p
                            .extra_str("userAgent")
                            .unwrap_or_else(|| crate::auth::antigravity::USER_AGENT.into()),
                    }
                } else {
                    gemini::Auth::ApiKey(key.clone().ok_or_else(|| setup(format!("{} needs an API key", p.id)))?)
                };
                Arc::new(gemini::Gemini::new(&p.id, &p.base_url, auth, trace))
            }
            Api::Copilot => {
                let token = self.auth.access_token(&p.id, account).await?;
                Arc::new(crate::copilot::Copilot::new(token, trace))
            }
        };
        Ok((adapter, key))
    }

    /// Where an API-key provider's key comes from, without reading it (L2.25).
    pub fn key_source(&self, p: &Profile) -> Option<String> {
        if self.cli_key.as_ref().is_some_and(|(id, _)| *id == p.id) {
            return Some("--api-key".into());
        }
        if self.auth.stored_key(&p.id).is_some() {
            return Some("novacode login (auth.json)".into());
        }
        if p.api_key.is_some() {
            return Some(format!("settings providers.{}.apiKey", p.id));
        }
        if let Some(v) = p.env.iter().find(|v| std::env::var(v).is_ok_and(|k| !k.is_empty())) {
            return Some(format!("env {v}"));
        }
        p.key_optional.then(|| "local server, no key needed".into())
    }

    /// Key order (pi): `--api-key`, then `novacode login`, then settings, then env.
    fn key(&self, p: &Profile) -> Result<Option<String>, ConnectError> {
        if let Some((_, k)) = self.cli_key.as_ref().filter(|(id, _)| *id == p.id) {
            return Ok(Some(clean_key(&auth::resolve_key(k)?)));
        }
        if let Some(v) = self.auth.stored_key(&p.id).or_else(|| p.api_key.clone()) {
            return Ok(Some(clean_key(&auth::resolve_key(&v)?)));
        }
        if let Some(k) = p
            .env
            .iter()
            .find_map(|v| std::env::var(v).ok().map(|k| clean_key(&k)).filter(|k| !k.is_empty()))
        {
            return Ok(Some(k));
        }
        if p.key_optional {
            return Ok(None);
        }
        let env = p.env.first().map(String::as_str).unwrap_or("its key env var");
        Err(setup(format!(
            "{} needs an API key: run `novacode login {}`, or set {env}",
            p.id, p.id
        )))
    }
}

/// A pasted key can carry invisible characters (zero-width spaces, a BOM, a
/// newline) that make the vendor reject it (jcode #376): drop them.
fn clean_key(k: &str) -> String {
    k.chars()
        .filter(|c| {
            !c.is_whitespace() && !c.is_control() && !matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}')
        })
        .collect()
}

/// Where the refreshed models.dev copy lives (L2.10).
pub fn cache_path(home: &Path) -> std::path::PathBuf {
    home.join("cache").join("models-dev.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers(settings: &str) -> (tempfile::TempDir, Arc<Providers>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, settings).unwrap();
        let s = Settings::load(&path, None, false);
        let p = Arc::new(Providers::load(dir.path(), &s));
        (dir, p)
    }

    #[test]
    fn routes_resolve_aliases_levels_and_metadata() {
        let (_d, p) = providers("{}");
        let r = p.resolve("anthropic/claude-opus-4-5:high").unwrap();
        assert_eq!(
            (r.model.as_str(), r.level),
            ("claude-opus-4-5", Some(ThinkingLevel::High))
        );
        assert!(r.info.known && r.info.context >= 200_000);
        let r = p.resolve("claude/claude-opus").unwrap();
        assert!(r.model.starts_with("claude-opus-"), "alias resolved: {}", r.model);
        assert_eq!(r.api, Api::Anthropic);
        assert!(
            p.resolve("nope/x")
                .unwrap_err()
                .to_string()
                .contains("unknown provider")
        );
        assert!(p.resolve("claude-opus").is_err());
        let r = p.resolve("deepseek/brand-new-model").unwrap();
        assert!(!r.info.known, "unknown models still route, with defaults");
    }

    #[test]
    fn keys_lose_invisible_characters() {
        assert_eq!(super::clean_key("\u{FEFF}sk-ab\u{200B}c \n"), "sk-abc");
    }

    #[tokio::test]
    async fn keys_come_from_settings_or_env() {
        let (_d, p) = providers(r#"{ "providers": { "groq": { "apiKey": "gsk-test" } } }"#);
        assert!(p.connected(&p.profiles["groq"]));
        let (_, key) = p.adapter("groq", None).await.unwrap();
        assert_eq!(key.as_deref(), Some("gsk-test"));
        assert!(p.connected(&p.profiles["ollama"]), "local server needs no key");
        let err = p.adapter("sambanova", None).await.err().unwrap().to_string();
        assert!(err.contains("SAMBANOVA_API_KEY"), "{err}");
    }

    #[tokio::test]
    async fn key_order_is_cli_then_login_then_settings() {
        let (_d, p) = providers(r#"{ "providers": { "groq": { "apiKey": "gsk-settings" } } }"#);
        let p = Arc::try_unwrap(p).ok().unwrap();
        p.auth.put_key("groq", "gsk-login\n").await.unwrap();
        p.auth.put_key("sambanova", "$NOVACODE_TEST_UNSET_VAR").await.unwrap();
        assert!(p.connected(&p.profiles["sambanova"]), "a saved key connects");
        let p = Arc::new(p);
        assert_eq!(p.adapter("groq", None).await.unwrap().1.as_deref(), Some("gsk-login"));
        assert!(p.auth.secrets().iter().any(|(k, _)| k == "gsk-login\n"));
        assert!(
            !p.auth.secrets().iter().any(|(k, _)| k.starts_with('$')),
            "indirections aren't secrets"
        );
        let mut p = Arc::try_unwrap(p).ok().unwrap();
        p.cli_key = Some(("groq".into(), "gsk-cli".into()));
        assert_eq!(p.key_source(&p.profiles["groq"]).as_deref(), Some("--api-key"));
        let p = Arc::new(p);
        assert_eq!(p.adapter("groq", None).await.unwrap().1.as_deref(), Some("gsk-cli"));
        assert!(p.auth.remove("groq", None).await.unwrap(), "logout drops the key");
        assert!(p.auth.stored_key("groq").is_none());
    }
}
