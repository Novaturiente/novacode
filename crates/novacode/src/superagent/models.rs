//! Worker models (L10.9 "Models"): each tier is a model **family** per provider, resolved
//! to the newest model of that family (models.dev `release_date`) that the provider offers,
//! is logged in and not denied. Workers follow super's current provider; reviews use
//! super's exact current model and thinking level.

use futures_util::future::BoxFuture;
use novacode_ai::catalog::{ModelInfo, strip_date};
use novacode_ai::connect::{Providers, split_level};
use novacode_base::settings::Settings;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The task tiers, weakest first.
pub const TIERS: [&str; 3] = ["quick", "standard", "deep"];

/// Thinking levels a live id may end in (Antigravity: `gemini-3.8-flash-high`), lowest first.
const LEVELS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];

/// The shipped presets: per provider, each tier's model family and thinking level.
/// A tier's model family and thinking level.
type Preset = (&'static str, Option<&'static str>);

const PRESETS: [(&str, [Preset; 3]); 3] = [
    (
        "claude",
        [
            ("claude-haiku", None),
            ("claude-sonnet", None),
            ("claude-opus", Some("high")),
        ],
    ),
    (
        "codex",
        [("gpt-luna", None), ("gpt-sol", None), ("gpt-astra", Some("high"))],
    ),
    (
        "antigravity",
        [
            ("gemini-flash", Some("medium")),
            ("gemini-flash", Some("high")),
            ("gemini", None), // Argon's catalog family.
        ],
    ),
];

/// A live list is fetched again after this long; a failed fetch is retried after [`RETRY`].
const LIVE_TTL: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(300);
/// Usable models the state block lists, newest first.
const LISTED: usize = 20;

/// The provider of a `provider/model` spec.
pub fn provider(spec: &str) -> &str {
    spec.split('/').next().unwrap_or(spec)
}

/// What the resolver needs to know about providers (tests fake it).
pub trait Source: Send + Sync {
    /// The provider's catalog models; `None`: an unknown provider.
    fn catalog(&self, provider: &str) -> Option<Vec<ModelInfo>>;
    /// Has a login or key.
    fn logged_in(&self, provider: &str) -> bool;
    /// Why `models.deny` blocks the model, if it does.
    fn denied(&self, provider: &str, id: &str) -> Option<String>;
    /// The provider's live model list.
    fn fetch(&self, provider: &str) -> BoxFuture<'static, Result<Vec<String>, String>>;
}

/// The real [`Source`]: novacode's providers.
pub struct Real(pub Arc<Providers>);

impl Source for Real {
    fn catalog(&self, provider: &str) -> Option<Vec<ModelInfo>> {
        if provider == "faux" {
            return Some(vec![ModelInfo::unknown("script")]);
        }
        let p = self.0.profile(provider).ok()?;
        Some(self.0.catalog.models(p.catalog_id()).cloned().collect())
    }
    fn logged_in(&self, provider: &str) -> bool {
        provider == "faux" || self.0.profile(provider).is_ok_and(|p| self.0.connected(p))
    }
    fn denied(&self, provider: &str, id: &str) -> Option<String> {
        // Aliases are checked as what they resolve to.
        let r = self.0.resolve(&format!("{provider}/{id}")).ok()?;
        self.0.allow(&r).err().map(|e| e.to_string())
    }
    fn fetch(&self, provider: &str) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let (p, id) = (self.0.clone(), provider.to_string());
        Box::pin(async move {
            if id == "faux" {
                return Ok(vec!["script".into()]);
            }
            let (a, _) = p.adapter(&id, None).await.map_err(|e| e.to_string())?;
            a.list_models().await
        })
    }
}

/// `super.tiers`: `<provider>.<tier>` overrides, and the old flat `<tier>` form, which
/// overrides that tier on every provider. Legacy `review` entries are parsed but unused.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    flat: BTreeMap<String, String>,
    per: BTreeMap<String, BTreeMap<String, String>>,
}

impl Overrides {
    pub fn from_settings(s: &Settings) -> Self {
        Self::parse(&s.get::<Value>("super.tiers").unwrap_or_default())
    }

    /// An entry is a model spec or `{model, note}`.
    pub fn parse(v: &Value) -> Self {
        let model = |v: &Value| v.as_str().or(v["model"].as_str()).map(String::from);
        let mut o = Overrides::default();
        for (k, v) in v.as_object().into_iter().flatten() {
            if TIERS.contains(&k.as_str()) || k == "review" {
                o.flat.extend(model(v).map(|m| (k.clone(), m)));
            } else if let Some(tiers) = v.as_object() {
                let per = o.per.entry(k.clone()).or_default();
                per.extend(tiers.iter().filter_map(|(t, v)| Some((t.clone(), model(v)?))));
            }
        }
        o
    }

    fn get(&self, provider: &str, tier: &str) -> Option<&String> {
        self.per
            .get(provider)
            .and_then(|m| m.get(tier))
            .or_else(|| self.flat.get(tier))
    }
}

/// When a live list was fetched, and the list (`None`: the fetch failed).
type Fetched = (Instant, Option<Vec<String>>);

/// The resolver, with each provider's live model list cached.
pub struct Models {
    source: Arc<dyn Source>,
    overrides: Overrides,
    /// Provider → when fetched, and the list (`None`: the fetch failed; the catalog is used).
    live: Mutex<HashMap<String, Fetched>>,
}

impl Models {
    pub fn new(source: Arc<dyn Source>, overrides: Overrides) -> Self {
        Models {
            source,
            overrides,
            live: Mutex::default(),
        }
    }

    pub fn real(p: Arc<Providers>, s: &Settings) -> Self {
        Self::new(Arc::new(Real(p)), Overrides::from_settings(s))
    }

    /// Fetch the provider's live list when there is none or it is old. A failed fetch
    /// leaves the catalog in use and is retried after a while.
    pub async fn refresh(&self, provider: &str) {
        let fresh = self.live.lock().unwrap().get(provider).is_some_and(|(at, ids)| {
            at.elapsed()
                < match ids {
                    Some(_) => LIVE_TTL,
                    None => RETRY,
                }
        });
        if fresh || !self.source.logged_in(provider) || self.source.catalog(provider).is_none() {
            return;
        }
        let r = self.source.fetch(provider).await;
        if let Err(e) = &r {
            tracing::warn!(provider, %e, "super: the live model list failed; using the catalog");
        }
        self.live
            .lock()
            .unwrap()
            .insert(provider.to_string(), (Instant::now(), r.ok()));
    }

    /// [`Self::refresh`] for super's provider and every logged-in preset provider.
    pub async fn warm(&self, own: &str) {
        let mut ps: Vec<&str> = PRESETS.iter().map(|(p, _)| *p).collect();
        if !ps.contains(&provider(own)) {
            ps.push(provider(own));
        }
        futures_util::future::join_all(ps.into_iter().map(|p| self.refresh(p))).await;
    }

    /// The provider's usable models (logged in, not denied): its live list when fetched,
    /// else its catalog; each with the catalog entry of its base id.
    pub fn usable(&self, provider: &str) -> Vec<(String, Option<ModelInfo>)> {
        let Some(cat) = self.source.catalog(provider) else {
            return vec![];
        };
        if !self.source.logged_in(provider) {
            return vec![];
        }
        let live = self.live.lock().unwrap().get(provider).and_then(|(_, l)| l.clone());
        let ids = live.unwrap_or_else(|| cat.iter().map(|m| m.id.clone()).collect());
        ids.into_iter()
            .filter(|id| self.source.denied(provider, id).is_none())
            .map(|id| {
                let info = base_info(&cat, &id);
                (id, info)
            })
            .collect()
    }

    /// The newest usable model of `family`, at `level`: `provider/id[:level]`.
    fn newest(&self, provider: &str, family: &str, level: Option<&str>) -> Result<String, String> {
        if !self.source.logged_in(provider) {
            return Err(format!("`{provider}` is not logged in"));
        }
        let usable = self.usable(provider);
        // Base id → its catalog entry and the ids offering it (itself or level variants).
        let mut bases: BTreeMap<String, (ModelInfo, Vec<String>)> = BTreeMap::new();
        for (id, info) in usable {
            if let Some(info) = info.filter(|i| matches_family(provider, family, i)) {
                bases
                    .entry(info.id.clone())
                    .or_insert_with(|| (info, vec![]))
                    .1
                    .push(id);
            }
        }
        let mut ranked: Vec<(ModelInfo, Vec<String>)> = bases.into_values().collect();
        ranked.sort_by_key(|(i, _)| std::cmp::Reverse(rank(i)));
        for (info, ids) in ranked {
            let variant = |l: &str| ids.iter().find(|id| **id == format!("{}-{l}", info.id));
            let plain = ids.iter().find(|id| **id == info.id);
            let pick = match level {
                Some(l) => variant(l).cloned().or_else(|| plain.map(|p| format!("{p}:{l}"))),
                None => plain.or_else(|| LEVELS.iter().find_map(|l| variant(l))).cloned(),
            };
            if let Some(id) = pick {
                return Ok(format!("{provider}/{id}"));
            }
        }
        Err(format!(
            "no `{family}` model is usable on `{provider}` (offered, not denied)"
        ))
    }

    /// The model for `tier` on `provider`: the user's override, else the preset. A provider
    /// without presets or an override uses `own` (super's model), with a notice.
    pub fn resolve(&self, provider: &str, tier: &str, own: &str) -> Result<(String, Option<String>), String> {
        if let Some(m) = self.overrides.get(provider, tier) {
            return Ok((m.clone(), None));
        }
        let i = TIERS
            .iter()
            .position(|t| *t == tier)
            .ok_or_else(|| format!("no tier `{tier}`: {}", TIERS.join(", ")))?;
        match PRESETS.iter().find(|(p, _)| *p == provider) {
            Some((_, tiers)) => {
                let (family, level) = tiers[i];
                self.newest(provider, family, level)
                    .or_else(|e| {
                        if provider == "antigravity" && tier == "deep" {
                            self.newest(provider, "gemini-flash", Some("high"))
                        } else {
                            Err(e)
                        }
                    })
                    .map(|m| (m, None))
            }
            None => Ok((
                own.to_string(),
                Some(format!(
                    "no model presets for provider `{provider}`: the `{tier}` tier uses your own model {own} \
                     (set `super.tiers.{provider}.{tier}` to change it)"
                )),
            )),
        }
    }

    /// Reviews use super's exact current model and thinking, never worker tier overrides.
    /// Keep the same login and deny checks as named worker models.
    pub fn review(&self, own: &str) -> Result<String, String> {
        self.allowed(own)?;
        Ok(own.to_string())
    }

    /// The tier a model is on: by its family in its provider's presets, else `standard`.
    pub fn tier_of(&self, spec: &str) -> &'static str {
        let (m, level) = split_level(spec);
        let Some((p, id)) = m.split_once('/') else {
            return "standard";
        };
        let info = self.source.catalog(p).and_then(|cat| base_info(&cat, id));
        let Some((_, tiers)) = PRESETS.iter().find(|(x, _)| *x == p) else {
            return "standard";
        };
        let level = level.map(|l| l.as_str().to_string()).or_else(|| {
            LEVELS
                .iter()
                .find(|l| id.ends_with(&format!("-{l}")))
                .map(|l| l.to_string())
        });
        let hits: Vec<usize> = (0..3)
            .filter(|i| info.as_ref().is_some_and(|info| matches_family(p, tiers[*i].0, info)))
            .collect();
        hits.iter()
            .find(|i| tiers[**i].1 == level.as_deref())
            .or(hits.first())
            .map_or("standard", |i| TIERS[*i])
    }

    /// A model super or the user named: the provider exists and is logged in, the model is
    /// not denied and is offered (or is a family alias).
    pub async fn check(&self, spec: &str) -> Result<(), String> {
        self.refresh(provider(spec.trim())).await;
        self.allowed(spec)
    }

    /// [`Self::check`] against the lists at hand (no fetch).
    pub fn allowed(&self, spec: &str) -> Result<(), String> {
        let (m, _) = split_level(spec.trim());
        let (p, id) = m
            .split_once('/')
            .ok_or_else(|| format!("`{spec}`: use provider/model, e.g. claude/claude-sonnet-5-5"))?;
        let Some(cat) = self.source.catalog(p) else {
            return Err(format!("`{spec}`: unknown provider `{p}`"));
        };
        if !self.source.logged_in(p) {
            return Err(format!("`{spec}`: provider `{p}` is not logged in"));
        }
        if let Some(e) = self.source.denied(p, id) {
            return Err(e);
        }
        let ids: Vec<String> = self.usable(p).into_iter().map(|(id, _)| id).collect();
        let alias = cat.iter().any(|i| i.family.as_deref() == Some(id));
        if !ids.is_empty() && !ids.iter().any(|i| i == id) && !alias {
            return Err(format!(
                "`{spec}`: `{p}` has no model `{id}`; the state block's Models section lists its usable ones"
            ));
        }
        Ok(())
    }

    /// The state block's Models section for super on `own`: what each tier resolves to on
    /// super's provider, and its usable models with prices.
    pub fn section(&self, own: &str) -> String {
        let p = provider(own);
        let short = |m: &str| m.strip_prefix(&format!("{p}/")).unwrap_or(m).to_string();
        let mut s = format!("\n## Models\nYour provider: {p} (you run on {own}). Tasks get a tier on it:\n");
        let mut notice = None;
        for t in TIERS {
            match self.resolve(p, t, own) {
                Ok((m, n)) => {
                    s += &format!("- {t}: {}\n", short(&m));
                    notice = notice.or(n);
                }
                Err(e) => s += &format!("- {t}: none ({e})\n"),
            }
        }
        if let Some(n) = notice {
            s += &format!("Note: {n}\n");
        }
        let mut usable = self.usable(p);
        usable.sort_by_key(|(_, i)| std::cmp::Reverse(i.as_ref().map(rank)));
        if usable.is_empty() {
            return s;
        }
        s += "Usable models (USD per million tokens in/out):";
        let n = usable.len();
        for (id, info) in usable.into_iter().take(LISTED) {
            let price = info
                .and_then(|i| i.cost)
                .map_or("no price".to_string(), |c| format!("${}/${}", c.input, c.output));
            s += &format!(" {id} {price};");
        }
        if n > LISTED {
            s += &format!(" (+{} older)", n - LISTED);
        }
        s.push('\n');
        s
    }
}

/// Antigravity's deep preset requires Argon, not every model in the broad `gemini` family.
fn matches_family(provider: &str, family: &str, info: &ModelInfo) -> bool {
    info.family.as_deref() == Some(family)
        && (provider != "antigravity"
            || family != "gemini"
            || (info.id.starts_with("gemini-") && strip_date(&info.id).ends_with("-argon")))
}

/// Newer first: release date, then not a `-latest` pointer, then undated, then the id.
fn rank(i: &ModelInfo) -> (Option<String>, bool, bool, String) {
    (
        i.release.clone(),
        !i.id.ends_with("-latest"),
        strip_date(&i.id) == i.id,
        i.id.clone(),
    )
}

/// The catalog entry of a live id: itself, without its date, or without a level suffix.
fn base_info(cat: &[ModelInfo], id: &str) -> Option<ModelInfo> {
    let find = |x: &str| cat.iter().find(|m| m.id == x).cloned();
    find(id).or_else(|| find(strip_date(id))).or_else(|| {
        LEVELS
            .iter()
            .find_map(|l| id.strip_suffix(&format!("-{l}")))
            .and_then(find)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use novacode_ai::catalog::Catalog;

    /// A faked [`Source`]: the bundled catalog for claude, codex and antigravity, any
    /// other provider known with an empty catalog.
    pub(crate) struct Fake {
        pub logged: Vec<String>,
        pub deny: Vec<String>,
        pub live: HashMap<String, Result<Vec<String>, String>>,
        pub fetches: Mutex<Vec<String>>,
        catalog: Catalog,
        extra: Vec<ModelInfo>,
    }

    impl Fake {
        pub(crate) fn new(logged: &[&str]) -> Fake {
            Fake {
                logged: logged.iter().map(|s| s.to_string()).collect(),
                deny: vec![],
                live: HashMap::new(),
                fetches: Mutex::default(),
                catalog: Catalog::load(std::path::Path::new("/nonexistent"), None),
                extra: vec![],
            }
        }
    }

    impl Source for Fake {
        fn catalog(&self, provider: &str) -> Option<Vec<ModelInfo>> {
            let id = match provider {
                "claude" => "anthropic",
                "codex" => "openai",
                "antigravity" => "google",
                "nowhere" => return None,
                _ => return Some(vec![]),
            };
            Some(self.catalog.models(id).cloned().chain(self.extra.clone()).collect())
        }
        fn logged_in(&self, provider: &str) -> bool {
            self.logged.iter().any(|l| l == provider || l == "*")
        }
        fn denied(&self, provider: &str, id: &str) -> Option<String> {
            (provider == "denied" || self.deny.iter().any(|d| id.starts_with(d.as_str())))
                .then(|| format!("{provider}/{id} is denied by models.deny"))
        }
        fn fetch(&self, provider: &str) -> BoxFuture<'static, Result<Vec<String>, String>> {
            self.fetches.lock().unwrap().push(provider.to_string());
            let r = self.live.get(provider).cloned().unwrap_or(Err("offline".into()));
            Box::pin(async move { r })
        }
    }

    /// The test resolver of the super tests: every provider logged in, each flat tier
    /// (and `review`) overridden to `faux/<tier>`.
    pub(crate) fn flat() -> Models {
        flat_with("faux/review")
    }

    /// [`flat`] with `review` overridden to `review`.
    pub(crate) fn flat_with(review: &str) -> Models {
        let o = serde_json::json!({
            "quick": "faux/quick", "standard": "faux/standard", "deep": "faux/deep", "review": {"model": review}
        });
        Models::new(Arc::new(Fake::new(&["*"])), Overrides::parse(&o))
    }

    fn models(f: Fake, o: Value) -> Models {
        Models::new(Arc::new(f), Overrides::parse(&o))
    }

    fn tier(m: &Models, p: &str, t: &str) -> Result<String, String> {
        m.resolve(p, t, "faux/script").map(|(m, _)| m)
    }

    #[tokio::test]
    async fn presets_pick_the_newest_of_each_family_from_the_live_list() {
        let mut f = Fake::new(&["claude", "codex"]);
        f.live.insert(
            "claude".into(),
            Ok([
                "claude-haiku-4-5",
                "claude-sonnet-4-6",
                "claude-sonnet-5",
                "claude-opus-4-8",
                "claude-opus-5",
            ]
            .map(String::from)
            .to_vec()),
        );
        let m = models(f, Value::Null);
        // Before a fetch: the catalog.
        assert_eq!(tier(&m, "claude", "standard").unwrap(), "claude/claude-sonnet-5-5");
        m.refresh("claude").await;
        assert_eq!(tier(&m, "claude", "quick").unwrap(), "claude/claude-haiku-4-5");
        assert_eq!(tier(&m, "claude", "standard").unwrap(), "claude/claude-sonnet-5");
        assert_eq!(tier(&m, "claude", "deep").unwrap(), "claude/claude-opus-5:high");
        // codex's fetch fails: the catalog.
        m.refresh("codex").await;
        assert_eq!(tier(&m, "codex", "quick").unwrap(), "codex/gpt-6-luna");
        assert_eq!(tier(&m, "codex", "standard").unwrap(), "codex/gpt-6.1-sol");
        assert_eq!(tier(&m, "codex", "deep").unwrap(), "codex/gpt-6-astra:high");
    }

    #[tokio::test]
    async fn the_live_list_is_cached() {
        let mut f = Fake::new(&["claude"]);
        f.live.insert("claude".into(), Ok(vec!["claude-opus-5".into()]));
        let f = Arc::new(f);
        let m = Models::new(f.clone(), Overrides::default());
        m.refresh("claude").await;
        m.refresh("claude").await;
        m.warm("claude/claude-opus-5").await;
        assert_eq!(
            *f.fetches.lock().unwrap(),
            ["claude"],
            "fetched once; others are not logged in"
        );
    }

    #[test]
    fn denied_and_not_logged_in_models_are_skipped() {
        let mut f = Fake::new(&["claude"]);
        f.deny = vec!["claude-sonnet-5-5".into(), "claude-haiku".into()];
        let m = models(f, Value::Null);
        assert_eq!(tier(&m, "claude", "standard").unwrap(), "claude/claude-sonnet-5");
        let e = tier(&m, "claude", "quick").unwrap_err();
        assert!(e.contains("claude-haiku") && e.contains("usable"), "{e}");
        let e = tier(&m, "codex", "quick").unwrap_err();
        assert_eq!(e, "`codex` is not logged in");
    }

    #[tokio::test]
    async fn antigravity_level_variants_map_to_their_base_and_tiers_pick_their_level() {
        let mut f = Fake::new(&["antigravity"]);
        let live = [
            "gemini-3.7-flash-high",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-low",
            "gemini-3.5-flash-lite",
            "claude-sonnet-4-6",
        ];
        f.live.insert("antigravity".into(), Ok(live.map(String::from).to_vec()));
        let m = models(f, Value::Null);
        m.refresh("antigravity").await;
        assert_eq!(
            tier(&m, "antigravity", "quick").unwrap(),
            "antigravity/gemini-3.8-flash-medium"
        );
        assert_eq!(
            tier(&m, "antigravity", "standard").unwrap(),
            "antigravity/gemini-3.8-flash-high"
        );
        assert_eq!(
            tier(&m, "antigravity", "deep").unwrap(),
            "antigravity/gemini-3.8-flash-high"
        );
        assert_eq!(m.tier_of("antigravity/gemini-3.8-flash-high"), "standard");
        assert_eq!(m.tier_of("antigravity/gemini-3.8-flash-medium"), "quick");
        assert_eq!(m.tier_of("antigravity/gemini-3.8-flash:high"), "standard");
        assert_eq!(m.tier_of("antigravity/gemini-3.8-flash:medium"), "quick");
        assert_eq!(m.tier_of("antigravity/gemini-4-argon"), "deep");
        assert_eq!(m.tier_of("claude/claude-opus-5:high"), "deep");
        assert_eq!(m.tier_of("faux/script"), "standard");
    }

    #[tokio::test]
    async fn antigravity_deep_excludes_other_gemini_family_members() {
        for offered in [false, true] {
            let mut f = Fake::new(&["antigravity"]);
            let mut other = ModelInfo::unknown("gemini-5-other");
            other.family = Some("gemini".into());
            other.release = Some("2027-01-01".into());
            f.extra.push(other);
            let mut live = vec!["gemini-5-other".into(), "gemini-3.8-flash-high".into()];
            if offered {
                live.push("gemini-4-argon".into());
            }
            f.live.insert("antigravity".into(), Ok(live));
            let m = models(f, Value::Null);
            m.refresh("antigravity").await;
            assert_eq!(m.tier_of("antigravity/gemini-5-other"), "standard");
            assert_eq!(
                tier(&m, "antigravity", "deep").unwrap(),
                if offered {
                    "antigravity/gemini-4-argon"
                } else {
                    "antigravity/gemini-3.8-flash-high"
                }
            );
        }
    }

    #[tokio::test]
    async fn antigravity_argon_is_deep_only_when_usable() {
        for denied in [false, true] {
            let mut f = Fake::new(&["antigravity"]);
            f.live.insert(
                "antigravity".into(),
                Ok(["gemini-4-argon", "gemini-3.8-flash"].map(String::from).to_vec()),
            );
            if denied {
                f.deny.push("gemini-4-argon".into());
            }
            let m = models(f, Value::Null);
            m.refresh("antigravity").await;
            assert_eq!(
                tier(&m, "antigravity", "quick").unwrap(),
                "antigravity/gemini-3.8-flash:medium"
            );
            assert_eq!(
                tier(&m, "antigravity", "standard").unwrap(),
                "antigravity/gemini-3.8-flash:high"
            );
            assert_eq!(
                tier(&m, "antigravity", "deep").unwrap(),
                if denied {
                    "antigravity/gemini-3.8-flash:high"
                } else {
                    "antigravity/gemini-4-argon"
                }
            );
        }
        let mut f = Fake::new(&["antigravity"]);
        f.deny = vec!["gemini".into()];
        assert!(tier(&models(f, Value::Null), "antigravity", "deep").is_err());
        let m = models(Fake::new(&[]), Value::Null);
        for t in TIERS {
            assert_eq!(
                tier(&m, "antigravity", t).unwrap_err(),
                "`antigravity` is not logged in"
            );
        }
        let m = models(
            Fake::new(&[]),
            serde_json::json!({
                "deep": "faux/flat",
                "antigravity": {"deep": "faux/per-provider"}
            }),
        );
        assert_eq!(tier(&m, "antigravity", "deep").unwrap(), "faux/per-provider");
        let m = models(Fake::new(&[]), serde_json::json!({"deep": "faux/flat"}));
        assert_eq!(tier(&m, "antigravity", "deep").unwrap(), "faux/flat");
    }

    #[test]
    fn overrides_per_provider_win_over_the_old_flat_form_which_wins_over_presets() {
        let o = serde_json::json!({
            "quick": {"model": "faux/script", "note": "old form"},
            "claude": {"quick": "claude/claude-haiku-4-5", "deep": {"model": "claude/claude-opus-4-8"}}
        });
        let m = models(Fake::new(&["claude", "codex"]), o);
        assert_eq!(tier(&m, "claude", "quick").unwrap(), "claude/claude-haiku-4-5");
        assert_eq!(tier(&m, "claude", "deep").unwrap(), "claude/claude-opus-4-8");
        assert_eq!(tier(&m, "codex", "quick").unwrap(), "faux/script");
        assert_eq!(tier(&m, "claude", "standard").unwrap(), "claude/claude-sonnet-5-5");
        assert!(tier(&m, "claude", "review").unwrap_err().contains("no tier `review`"));
    }

    #[test]
    fn an_unknown_provider_without_override_uses_super_s_model_with_a_notice() {
        let m = models(Fake::new(&["*"]), Value::Null);
        let (model, note) = m.resolve("openrouter", "deep", "openrouter/x/y").unwrap();
        assert_eq!(model, "openrouter/x/y");
        assert!(note.unwrap().contains("super.tiers.openrouter.deep"));
        let s = m.section("openrouter/x/y");
        assert!(s.contains("- deep: x/y") && s.contains("Note: no model presets"), "{s}");
    }

    #[test]
    fn reviews_follow_exact_current_model_and_ignore_worker_overrides() {
        let m = models(
            Fake::new(&["claude"]),
            serde_json::json!({
                "review": "codex/gpt-6-astra:high",
                "deep": "codex/gpt-6-astra:high",
                "claude": {"deep": "codex/gpt-6-astra:high"}
            }),
        );
        for own in [
            "claude/claude-sonnet-5:low",
            "claude/claude-opus-5:medium",
            "claude/claude-opus-5:off",
        ] {
            let model = m.review(own).unwrap();
            assert_eq!(model, own);
        }
    }

    #[test]
    fn reviews_keep_auth_and_deny_validation_without_fallback() {
        let mut f = Fake::new(&["claude"]);
        f.deny.push("claude-opus".into());
        let m = models(f, Value::Null);
        assert!(
            m.review("claude/claude-opus-5:high")
                .unwrap_err()
                .contains("models.deny")
        );
        assert!(m.review("codex/gpt-6.1-sol:low").unwrap_err().contains("not logged in"));
        assert!(m.review("nowhere/model:off").unwrap_err().contains("unknown provider"));
    }

    #[tokio::test]
    async fn named_models_must_exist_be_logged_in_and_not_denied() {
        let mut f = Fake::new(&["claude"]);
        f.deny = vec!["claude-opus-4-5".into()];
        f.live.insert(
            "claude".into(),
            Ok(vec!["claude-opus-5".into(), "claude-opus-4-5".into()]),
        );
        let m = models(f, Value::Null);
        m.check("claude/claude-opus-5:high").await.unwrap();
        m.check("claude/claude-opus").await.unwrap();
        let e = m.check("claude/claude-opus-4-5").await.unwrap_err();
        assert!(e.contains("models.deny"), "{e}");
        let e = m.check("claude/claude-sonnet-5-5").await.unwrap_err();
        assert!(e.contains("has no model `claude-sonnet-5-5`"), "{e}");
        let e = m.check("codex/gpt-6.1-sol").await.unwrap_err();
        assert!(e.contains("not logged in"), "{e}");
        let e = m.check("nowhere/x").await.unwrap_err();
        assert!(e.contains("unknown provider"), "{e}");
        assert!(m.check("opus").await.unwrap_err().contains("use provider/model"));
    }

    #[tokio::test]
    async fn the_section_lists_usable_models_with_prices_and_each_tier() {
        let mut f = Fake::new(&["claude"]);
        f.live.insert(
            "claude".into(),
            Ok([
                "claude-haiku-5-5",
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "claude-mystery-1",
            ]
            .map(String::from)
            .to_vec()),
        );
        let m = models(f, Value::Null);
        m.refresh("claude").await;
        let s = m.section("claude/claude-sonnet-5-5");
        for want in [
            "Your provider: claude",
            "- quick: claude-haiku-5-5\n",
            "- deep: claude-opus-5-5:high\n",
            "claude-opus-5-5 $4/$20;",
            "claude-mystery-1 no price;",
        ] {
            assert!(s.contains(want), "{want}: {s}");
        }
        assert!(
            s.find("claude-sonnet-5-5 $").unwrap() < s.find("claude-opus-5-5 $").unwrap(),
            "newest first: {s}"
        );
    }
}
