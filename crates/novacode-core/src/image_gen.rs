//! `generate_image` (L2.26): Gemini image models through the Antigravity login,
//! or a Gemini API key. Images are saved under `<cwd>/.novacode/images/`; only
//! their paths come back, so they cost no tokens until the model `read`s one.

use base64::Engine;
use futures_util::future::BoxFuture;
use novacode_ai::auth::{AuthStore, DEFAULT_ACCOUNT, antigravity};
use novacode_ai::connect::Providers;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MODELS: [&str; 3] = [
    "gemini-3.1-flash-image",
    "gemini-3-pro-image",
    "gemini-3-pro-image-preview",
];
const RATIOS: [&str; 10] = ["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"];

pub struct GenerateImage {
    pub auth: Arc<AuthStore>,
    pub providers: Arc<Providers>,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// What to draw: subject, style, composition, text to show.
    prompt: String,
    /// One of 1:1 (default), 2:3, 3:2, 3:4, 4:3, 4:5, 5:4, 9:16, 16:9, 21:9.
    aspect_ratio: Option<String>,
    /// How many images (1–4, default 1).
    n: Option<usize>,
    /// Images to edit or take after (paths).
    #[serde(default)]
    inputs: Vec<String>,
}

fn body(prompt: &str, ratio: &str, inputs: &[(String, String)]) -> Value {
    let mut parts = vec![json!({ "text": prompt })];
    parts.extend(
        inputs
            .iter()
            .map(|(mime, data)| json!({ "inlineData": { "mimeType": mime, "data": data } })),
    );
    json!({
        "contents": [{ "role": "user", "parts": parts }],
        "generationConfig": { "imageConfig": { "aspectRatio": ratio }, "candidateCount": 1 },
    })
}

/// (mime, base64) images and any text from a (possibly wrapped) answer.
fn images(v: &Value) -> (Vec<(String, String)>, Vec<String>) {
    let c = if v["response"].is_object() { &v["response"] } else { v };
    let (mut img, mut text) = (Vec::new(), Vec::new());
    for cand in c["candidates"].as_array().into_iter().flatten() {
        for p in cand["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(d) = p["inlineData"]["data"].as_str() {
                img.push((
                    p["inlineData"]["mimeType"].as_str().unwrap_or("image/png").to_string(),
                    d.to_string(),
                ));
            } else if let Some(t) = p["text"].as_str().filter(|t| !t.trim().is_empty()) {
                text.push(t.trim().to_string());
            }
        }
    }
    (img, text)
}

impl GenerateImage {
    /// One image (or an error), trying each model in turn.
    async fn one(&self, b: &Value) -> Result<(Vec<(String, String)>, Vec<String>, String), String> {
        let http = novacode_ai::http::client();
        let mut last =
            String::from("no image engine is set up: `novacode login antigravity`, or a Gemini key (GEMINI_API_KEY)");
        if let Ok(cred) = self.auth.credential(antigravity::PROVIDER, DEFAULT_ACCOUNT).await
            && let Some(project) = cred.project.clone()
        {
            for model in MODELS {
                let env = json!({
                    "project": project,
                    "model": model,
                    "request": b,
                    "requestType": "image_gen",
                    "userAgent": "antigravity",
                    "requestId": format!("image_gen/{}/1", novacode_base::new_id()),
                });
                let r = http
                    .post(format!("{}/v1internal:generateContent", antigravity::ENDPOINT))
                    .bearer_auth(&cred.access)
                    .header("content-type", "application/json")
                    .header("user-agent", antigravity::USER_AGENT)
                    .body(env.to_string())
                    .timeout(Duration::from_secs(180))
                    .send()
                    .await;
                match r {
                    Ok(r) if r.status().is_success() => {
                        let v: Value = serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or_default();
                        let (img, text) = images(&v);
                        if !img.is_empty() {
                            return Ok((img, text, format!("antigravity/{model}")));
                        }
                        last = format!(
                            "{model} returned no image{}",
                            text.first().map(|t| format!(": {t}")).unwrap_or_default()
                        );
                    }
                    Ok(r) => last = format!("{model}: HTTP {}", r.status().as_u16()),
                    Err(e) => last = format!("{model}: {e}"),
                }
            }
        }
        if let Some(key) = self.providers.api_key("google") {
            let model = "gemini-2.5-flash-image";
            let r = http
                .post(format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
                ))
                .header("x-goog-api-key", key)
                .header("content-type", "application/json")
                .body(b.to_string())
                .timeout(Duration::from_secs(180))
                .send()
                .await;
            match r {
                Ok(r) if r.status().is_success() => {
                    let v: Value = serde_json::from_slice(&r.bytes().await.unwrap_or_default()).unwrap_or_default();
                    let (img, text) = images(&v);
                    if !img.is_empty() {
                        return Ok((img, text, format!("google/{model}")));
                    }
                    last = format!("{model} returned no image");
                }
                Ok(r) => last = format!("{model}: HTTP {}", r.status().as_u16()),
                Err(e) => last = format!("{model}: {e}"),
            }
        }
        Err(last)
    }
}

fn slug(prompt: &str) -> String {
    let s: String = prompt
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s: Vec<&str> = s.split('-').filter(|w| !w.is_empty()).take(6).collect();
    if s.is_empty() { "image".into() } else { s.join("-") }
}

fn save(dir: &Path, stem: &str, i: usize, mime: &str, data: &str) -> Result<PathBuf, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())?;
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    };
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let p = dir.join(format!("{stem}-{i}.{ext}"));
    std::fs::write(&p, bytes).map_err(|e| e.to_string())?;
    Ok(p)
}

impl Tool for GenerateImage {
    fn name(&self) -> &str {
        "generate_image"
    }
    fn description(&self) -> &str {
        "Generate (or edit) images from a text prompt with a Gemini image model. Saves them under .novacode/images/ and returns their paths; `read` one to look at it."
    }
    fn schema(&self) -> Value {
        novacode_tools::schema_for::<Args>()
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match novacode_tools::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let ratio = a.aspect_ratio.as_deref().unwrap_or("1:1");
            if !RATIOS.contains(&ratio) {
                return ToolOutput::error(format!("aspect_ratio must be one of {}", RATIOS.join(", ")));
            }
            let mut inputs = Vec::new();
            for p in &a.inputs {
                let path = ctx.path(p);
                match std::fs::read(&path) {
                    Ok(b) => inputs.push((
                        novacode_tools::image::sniff(&b).unwrap_or("image/png").to_string(),
                        base64::engine::general_purpose::STANDARD.encode(b),
                    )),
                    Err(e) => return ToolOutput::error(format!("{}: {e}", path.display())),
                }
            }
            let b = body(&a.prompt, ratio, &inputs);
            let n = a.n.unwrap_or(1).clamp(1, 4);
            let runs = futures_util::future::join_all((0..n).map(|_| self.one(&b))).await;
            let stem = format!("{}-{}", novacode_core_ts(), slug(&a.prompt));
            let dir = ctx.cwd.join(".novacode").join("images");
            let (mut saved, mut notes, mut errors, mut used) = (Vec::new(), Vec::new(), Vec::new(), String::new());
            for r in runs {
                match r {
                    Ok((imgs, text, model)) => {
                        used = model;
                        notes.extend(text);
                        for (mime, data) in imgs {
                            match save(&dir, &stem, saved.len() + 1, &mime, &data) {
                                Ok(p) => saved.push(p),
                                Err(e) => errors.push(e),
                            }
                        }
                    }
                    Err(e) => errors.push(e),
                }
            }
            if saved.is_empty() {
                return ToolOutput::error(format!("no image generated: {}", errors.join("; ")));
            }
            let mut out = format!("Saved {} image(s) with {used}:\n", saved.len());
            for p in &saved {
                out += &format!("{}\n", p.strip_prefix(&ctx.cwd).unwrap_or(p).display());
            }
            if !notes.is_empty() {
                out += &format!("\nModel notes: {}", notes.join(" "));
            }
            if !errors.is_empty() {
                out += &format!("\nFailed: {}", errors.join("; "));
            }
            ToolOutput {
                details: Some(json!({ "images": saved })),
                ..ToolOutput::text(out.trim_end().to_string())
            }
        })
    }
}

/// UTC timestamp for file names (yyyymmdd-hhmmss).
fn novacode_core_ts() -> String {
    let s = crate::session::now_ms() / 1000;
    let m = crate::memory::utc_minute(s * 1000);
    format!("{}{:02}", m.replace(['-', ':', ' ', 'T'], ""), s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_names() {
        let v = json!({"response": {"candidates": [{"content": {"parts": [{"text": "here you go"}, {"inlineData": {"mimeType": "image/png", "data": "aGk="}}]}}]}});
        let (img, text) = images(&v);
        assert_eq!(img, [("image/png".to_string(), "aGk=".to_string())]);
        assert_eq!(text, ["here you go"]);
        assert_eq!(slug("A red Fox, at dawn!"), "a-red-fox-at-dawn");
        let d = tempfile::tempdir().unwrap();
        let p = save(d.path(), "x", 1, "image/png", "aGk=").unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"hi");
        assert_eq!(
            body("p", "16:9", &[])["generationConfig"]["imageConfig"]["aspectRatio"],
            "16:9"
        );
    }
}
