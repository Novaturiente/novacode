//! `web_fetch` and `web_search` (L5.13). Fetch returns markdown (5 MB download
//! cap; the L5.3 output guards cut it to size). Search tries, in order: Google
//! grounding through the Antigravity login, a Gemini API key, Brave, SearXNG.
//! No HTML scraping of search pages.

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use novacode_ai::auth::{AuthStore, DEFAULT_ACCOUNT, antigravity};
use novacode_ai::connect::Providers;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

const MAX_BYTES: usize = 5 * 1024 * 1024;

pub struct WebFetch;

#[derive(Deserialize, JsonSchema)]
struct FetchArgs {
    /// http(s) URL.
    url: String,
    /// markdown (default), text, or html (the raw page).
    #[serde(default)]
    format: Option<String>,
}

/// HTML → markdown without scripts, styles and page chrome.
pub fn to_markdown(html: &str) -> String {
    let conv = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "noscript", "svg", "nav", "footer", "iframe", "form",
        ])
        .build();
    let md = conv.convert(html).unwrap_or_else(|_| html.to_string());
    // Collapse the blank-line runs layouts leave behind.
    let mut out = String::with_capacity(md.len());
    let mut blank = 0;
    for l in md.lines() {
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

async fn get(url: &str) -> Result<(String, Vec<u8>, bool), String> {
    let resp = novacode_ai::http::client()
        .get(url)
        .header("user-agent", concat!("novacode/", env!("CARGO_PKG_VERSION")))
        .header(
            "accept",
            "text/html,text/markdown,text/plain,application/json;q=0.9,*/*;q=0.5",
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !status.is_success() {
        return Err(format!("HTTP {} from {url}", status.as_u16()));
    }
    let mut body = Vec::new();
    let mut cut = false;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("reading {url}: {e}"))?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            body.truncate(MAX_BYTES);
            cut = true;
            break;
        }
    }
    Ok((ctype, body, cut))
}

impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a web page or file over http(s) and return it as markdown (or plain text / raw html). Downloads stop at 5 MB."
    }
    fn schema(&self) -> Value {
        novacode_tools::schema_for::<FetchArgs>()
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: FetchArgs = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
                return ToolOutput::error("only http(s) URLs can be fetched");
            }
            let (ctype, body, cut) = match get(&a.url).await {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(e),
            };
            let textual = ctype.is_empty()
                || ctype.starts_with("text/")
                || ["json", "xml", "javascript", "yaml", "toml"]
                    .iter()
                    .any(|k| ctype.contains(k));
            if !textual {
                return ToolOutput::error(format!(
                    "{} is {ctype} ({} bytes), not text; download it with bash (curl -o) if you need the file",
                    a.url,
                    body.len()
                ));
            }
            let raw = String::from_utf8_lossy(&body).into_owned();
            let html =
                ctype.contains("html") || raw.trim_start().starts_with("<!") || raw.trim_start().starts_with("<html");
            let mut out = match (a.format.as_deref().unwrap_or("markdown"), html) {
                ("html", _) | (_, false) => raw,
                ("text", true) => html_text(&to_markdown(&raw)),
                (_, true) => to_markdown(&raw),
            };
            if cut {
                out.push_str("\n\n[cut at 5 MB]");
            }
            ToolOutput::text(out)
        })
    }
}

/// Markdown → plain-ish text: drop link targets and emphasis marks.
fn html_text(md: &str) -> String {
    let re = regex_lite_links(md);
    re.replace(['*', '_', '`'], "")
}

/// `[label](url)` → `label`.
fn regex_lite_links(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        match tail
            .find("](")
            .and_then(|j| tail[j + 2..].find(')').map(|k| (j, j + 2 + k)))
        {
            Some((j, end)) if !tail[..j].contains('\n') => {
                out.push_str(&tail[..j]);
                rest = &tail[end + 1..];
            }
            _ => {
                out.push('[');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

pub struct WebSearch {
    pub auth: Arc<AuthStore>,
    pub providers: Arc<Providers>,
    /// Setting `web.searxngUrl`.
    pub searxng: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// What to find out, as a question or keywords.
    query: String,
    /// Pages to read closely as part of the answer (grounded engines only).
    #[serde(default)]
    urls: Vec<String>,
}

const SEARCH_MODELS: [&str; 3] = ["gemini-3.5-flash-lite", "gemini-3.1-flash-lite", "gemini-3-flash"];

const BRIEF: &str = "You are a careful research assistant using Google Search grounding. Return a compact evidence brief with exactly these sections: ## Findings, ## Gaps, ## Next checks. Verify each claim inside the specific item it refers to; prefer primary documentation and exact names, versions and constraints; a fact no retrieved source supports goes under Gaps. Keep it short.";

fn grounded_request(query: &str, urls: &[String]) -> Value {
    let mut prompt = query.trim().to_string();
    let mut tools = vec![json!({"googleSearch": {}})];
    if !urls.is_empty() {
        prompt += &format!("\n\nURLs to analyse in detail:\n{}", urls.join("\n"));
        tools.push(json!({"urlContext": {}}));
    }
    json!({
        "systemInstruction": {"role": "user", "parts": [{"text": BRIEF}]},
        "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        "tools": tools,
    })
}

/// Text plus numbered sources from a (possibly wrapped) generateContent answer.
pub fn read_grounded(v: &Value) -> Option<String> {
    let c = v["response"]["candidates"][0].clone();
    let c = if c.is_null() { v["candidates"][0].clone() } else { c };
    let text: String = c["content"]["parts"]
        .as_array()?
        .iter()
        .filter(|p| p["thought"].as_bool() != Some(true))
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.trim().is_empty() {
        return None;
    }
    let mut out = text.trim().to_string();
    let g = &c["groundingMetadata"];
    if let Some(chunks) = g["groundingChunks"].as_array() {
        let srcs: Vec<String> = chunks
            .iter()
            .enumerate()
            .filter_map(|(i, ch)| {
                let w = &ch["web"];
                let uri = w["uri"].as_str()?;
                Some(format!("[{}] {} — {uri}", i + 1, w["title"].as_str().unwrap_or(uri)))
            })
            .collect();
        if !srcs.is_empty() {
            out += &format!("\n\nSources:\n{}", srcs.join("\n"));
        }
    }
    if let Some(q) = g["webSearchQueries"].as_array().filter(|q| !q.is_empty()) {
        let q: Vec<&str> = q.iter().filter_map(Value::as_str).collect();
        out += &format!("\n\nSearched: {}", q.join(" · "));
    }
    Some(out)
}

/// Grounding sources are Google redirect links: swap each for where it points,
/// read from the `Location` header without following it (5 s, in parallel).
async fn unredirect(text: String) -> String {
    const PREFIX: &str = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/";
    let links: Vec<String> = text
        .split_whitespace()
        .filter(|w| w.starts_with(PREFIX))
        .map(String::from)
        .collect();
    if links.is_empty() {
        return text;
    }
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
    else {
        return text;
    };
    let real = futures_util::future::join_all(links.iter().map(|l| {
        let c = client.clone();
        async move {
            let r = c.get(l).send().await.ok()?;
            r.headers().get("location")?.to_str().ok().map(String::from)
        }
    }))
    .await;
    let mut out = text;
    for (l, r) in links.iter().zip(real) {
        if let Some(r) = r {
            out = out.replace(l.as_str(), &r);
        }
    }
    out
}

impl WebSearch {
    async fn antigravity(&self, a: &SearchArgs) -> Option<Result<String, String>> {
        let cred = self
            .auth
            .credential(antigravity::PROVIDER, DEFAULT_ACCOUNT)
            .await
            .ok()?;
        let project = cred.project.clone()?;
        let mut last = String::new();
        for model in SEARCH_MODELS {
            let body = json!({
                "project": project,
                "model": model,
                "request": grounded_request(&a.query, &a.urls),
                "requestType": "agent",
                "userAgent": "antigravity",
                "requestId": format!("agent-{}", novacode_base::new_id()),
            });
            let resp = novacode_ai::http::client()
                .post(format!("{}/v1internal:generateContent", antigravity::ENDPOINT))
                .bearer_auth(&cred.access)
                .header("content-type", "application/json")
                .header("user-agent", antigravity::USER_AGENT)
                .body(body.to_string())
                .timeout(Duration::from_secs(60))
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let status = resp.status();
            let bytes = resp.bytes().await.unwrap_or_default();
            if !status.is_success() {
                // A withdrawn or busy model: the next one.
                last = format!(
                    "HTTP {}: {}",
                    status.as_u16(),
                    String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>()
                );
                continue;
            }
            let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            match read_grounded(&v) {
                Some(t) => return Some(Ok(unredirect(t).await)),
                None => last = "empty answer".into(),
            }
        }
        Some(Err(format!("Antigravity search failed: {last}")))
    }

    async fn gemini_key(&self, a: &SearchArgs) -> Option<Result<String, String>> {
        let key = self.providers.api_key("google")?;
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            "gemini-2.5-flash"
        );
        let r = novacode_ai::http::client()
            .post(url)
            .header("x-goog-api-key", key)
            .header("content-type", "application/json")
            .body(grounded_request(&a.query, &a.urls).to_string())
            .timeout(Duration::from_secs(60))
            .send()
            .await;
        Some(match r {
            Ok(r) if r.status().is_success() => {
                let v: Value = serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or_default();
                match read_grounded(&v) {
                    Some(t) => Ok(unredirect(t).await),
                    None => Err("Gemini search gave an empty answer".to_string()),
                }
            }
            Ok(r) => Err(format!("Gemini search: HTTP {}", r.status().as_u16())),
            Err(e) => Err(format!("Gemini search: {e}")),
        })
    }

    async fn brave(&self, a: &SearchArgs) -> Option<Result<String, String>> {
        let key = std::env::var("BRAVE_API_KEY").ok().filter(|k| !k.is_empty())?;
        let r = novacode_ai::http::client()
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[("q", a.query.as_str()), ("count", "10")])
            .header("x-subscription-token", key)
            .header("accept", "application/json")
            .timeout(Duration::from_secs(30))
            .send()
            .await;
        Some(match r {
            Ok(r) if r.status().is_success() => {
                let v: Value = serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or_default();
                Ok(list(v["web"]["results"].as_array(), "title", "url", "description"))
            }
            Ok(r) => Err(format!("Brave search: HTTP {}", r.status().as_u16())),
            Err(e) => Err(format!("Brave search: {e}")),
        })
    }

    async fn searxng(&self, a: &SearchArgs) -> Option<Result<String, String>> {
        let base = self.searxng.clone()?;
        let r = novacode_ai::http::client()
            .get(format!("{}/search", base.trim_end_matches('/')))
            .query(&[("q", a.query.as_str()), ("format", "json")])
            .timeout(Duration::from_secs(30))
            .send()
            .await;
        Some(match r {
            Ok(r) if r.status().is_success() => {
                let v: Value = serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or_default();
                Ok(list(v["results"].as_array(), "title", "url", "content"))
            }
            Ok(r) => Err(format!("SearXNG: HTTP {}", r.status().as_u16())),
            Err(e) => Err(format!("SearXNG: {e}")),
        })
    }
}

/// Result links as a numbered list (engines without a written answer).
fn list(items: Option<&Vec<Value>>, title: &str, url: &str, snippet: &str) -> String {
    let mut s = String::new();
    for (i, r) in items.into_iter().flatten().take(10).enumerate() {
        let _ = writeln!(
            s,
            "[{}] {} — {}\n    {}",
            i + 1,
            r[title].as_str().unwrap_or(""),
            r[url].as_str().unwrap_or(""),
            r[snippet].as_str().unwrap_or("").chars().take(300).collect::<String>()
        );
    }
    if s.is_empty() {
        "no results".into()
    } else {
        s.trim_end().into()
    }
}

impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web for current information, docs and facts. Returns a short evidence brief with numbered sources (or a list of results). Pass `urls` to have specific pages read as part of the answer."
    }
    fn schema(&self) -> Value {
        novacode_tools::schema_for::<SearchArgs>()
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: SearchArgs = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let mut errors = Vec::new();
            for r in [
                self.antigravity(&a).await,
                self.gemini_key(&a).await,
                self.brave(&a).await,
                self.searxng(&a).await,
            ]
            .into_iter()
            .flatten()
            {
                match r {
                    Ok(t) => return ToolOutput::text(t),
                    Err(e) => errors.push(e),
                }
            }
            if errors.is_empty() {
                return ToolOutput::error(
                    "no search engine is set up: `novacode login antigravity`, a Gemini key (GEMINI_API_KEY), BRAVE_API_KEY, or the setting web.searxngUrl",
                );
            }
            ToolOutput::error(errors.join("\n"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_and_grounding() {
        let md = to_markdown(
            "<html><head><style>x{}</style><script>evil()</script></head><body><nav>menu</nav><h1>Title</h1><p>Hello <a href=\"https://a.b\">link</a></p>\n\n\n<p>two</p></body></html>",
        );
        assert!(md.contains("# Title") && md.contains("[link](https://a.b)"), "{md}");
        assert!(
            !md.contains("evil") && !md.contains("menu") && !md.contains("\n\n\n"),
            "{md}"
        );
        assert_eq!(html_text("see [docs](https://x.y) and **bold**"), "see docs and bold");
        let v = json!({"response": {"candidates": [{"content": {"parts": [{"text": "plan", "thought": true}, {"text": "## Findings\nRust 1.97"}]},
            "groundingMetadata": {"groundingChunks": [{"web": {"uri": "https://r.org", "title": "rust"}}], "webSearchQueries": ["rust latest"]}}]}});
        assert_eq!(
            read_grounded(&v).unwrap(),
            "## Findings\nRust 1.97\n\nSources:\n[1] rust — https://r.org\n\nSearched: rust latest"
        );
    }
}
