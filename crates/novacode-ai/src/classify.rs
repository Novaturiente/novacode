//! Classifier models (L2.26): non-chat models that answer typed questions about JSON
//! state. Three wire formats, as pi supports them: TypeSafe's System One (also served by
//! OpenRouter and OpenCode), Cloudflare Workers AI (System One in Cloudflare's envelope),
//! and OpenAI's Decisions API. Results never throw: provider errors come back in
//! `stop_reason` / `error_message`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Wire {
    SystemOne,
    Cloudflare,
    Decisions,
}

#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub provider: &'static str,
    pub id: &'static str,
    pub name: &'static str,
    pub wire: Wire,
    #[serde(skip)]
    pub base_url: &'static str,
    /// Accepts `images` with the state.
    pub images: bool,
    /// USD per million input tokens (0 = unpriced).
    pub cost_input: f64,
}

/// Bundled classifier catalog (pi's list). shortcut: a code table, not catalog data; move
/// to models.json when models.dev lists decision models with base URLs.
pub const MODELS: &[Model] = &[
    m(
        "typesafe",
        "jev-latest",
        "Jev",
        Wire::SystemOne,
        "https://api.typesafe.ai/v1",
        false,
        0.0,
    ),
    m(
        "openrouter",
        "typesafe/jev-1.13",
        "Jev 1.13",
        Wire::SystemOne,
        "https://openrouter.ai/api/v1",
        false,
        0.042,
    ),
    m(
        "openrouter",
        "~typesafe/jev-latest",
        "Jev (latest)",
        Wire::SystemOne,
        "https://openrouter.ai/api/v1",
        false,
        0.042,
    ),
    m(
        "opencode",
        "jev-1.13",
        "Jev 1.13",
        Wire::SystemOne,
        "https://opencode.ai/inference/v1",
        false,
        0.042,
    ),
    m(
        "opencode",
        "jev-1.13-free",
        "Jev 1.13 Free",
        Wire::SystemOne,
        "https://opencode.ai/inference/v1",
        false,
        0.0,
    ),
    m(
        "cloudflare-workers-ai",
        "@cf/cloudflare/clef",
        "Clef",
        Wire::Cloudflare,
        CLOUDFLARE,
        false,
        0.24,
    ),
    m(
        "cloudflare-workers-ai",
        "@cf/cloudflare/clef-flash",
        "Clef Flash",
        Wire::Cloudflare,
        CLOUDFLARE,
        false,
        0.09,
    ),
    m(
        "cloudflare-workers-ai",
        "typesafe/jev",
        "Jev",
        Wire::Cloudflare,
        CLOUDFLARE,
        false,
        0.0,
    ),
    m(
        "openai",
        "gpt-6-luna",
        "GPT-6 Luna",
        Wire::Decisions,
        "https://api.openai.com/v1",
        true,
        0.1,
    ),
];

const CLOUDFLARE: &str = "https://api.cloudflare.com/client/v4/accounts/{account}/ai";

const fn m(
    provider: &'static str,
    id: &'static str,
    name: &'static str,
    wire: Wire,
    base_url: &'static str,
    images: bool,
    cost_input: f64,
) -> Model {
    Model {
        provider,
        id,
        name,
        wire,
        base_url,
        images,
        cost_input,
    }
}

/// `provider/id`, where the id may itself contain `/`.
/// A bare id works too when only one provider has it.
pub fn find(spec: &str) -> Option<&'static Model> {
    if let Some(m) = MODELS.iter().find(|m| format!("{}/{}", m.provider, m.id) == spec) {
        return Some(m);
    }
    let mut by_id = MODELS.iter().filter(|m| m.id == spec);
    match (by_id.next(), by_id.next()) {
        (Some(m), None) => Some(m),
        _ => None,
    }
}

/// Choice labels as `{label: meaning}`, or just `[label, …]`.
fn labels<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum L {
        Map(BTreeMap<String, String>),
        List(Vec<String>),
    }
    Ok(match L::deserialize(d)? {
        L::Map(m) => m,
        L::List(l) => l.into_iter().map(|k| (k, String::new())).collect(),
    })
}

/// Env vars holding a classifier provider's key, for providers without a chat profile.
pub fn key_env(provider: &str) -> &'static [&'static str] {
    match provider {
        "typesafe" => &["TYPESAFE_API_KEY"],
        "cloudflare-workers-ai" => &["CLOUDFLARE_API_KEY", "CLOUDFLARE_API_TOKEN"],
        _ => &[],
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Pick one label; `criteria` maps label → meaning.
    Choice {
        instructions: String,
        #[serde(deserialize_with = "labels")]
        criteria: BTreeMap<String, String>,
    },
    /// Ordered scale, lowest first.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    Bool {
        instructions: String,
        criteria: BoolCriteria,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BoolCriteria {
    #[serde(default)]
    pub r#true: String,
    #[serde(default)]
    pub r#false: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub data: String,
    #[serde(alias = "mime_type", rename = "mimeType")]
    pub mime_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Context {
    pub state: Map<String, Value>,
    #[serde(default)]
    pub images: Vec<Image>,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        confidence: f64,
    },
    Bool {
        probability: f64,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub provider: String,
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    /// `stop` or `error`.
    pub stop_reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Run one classification. `account` fills Cloudflare's URL.
pub async fn classify(http: &reqwest::Client, m: &Model, key: &str, account: Option<&str>, ctx: &Context) -> Outcome {
    let mut out = Outcome {
        provider: m.provider.into(),
        model: m.id.into(),
        answers: BTreeMap::new(),
        usage: None,
        stop_reason: "stop",
        error_message: None,
    };
    match run(http, m, key, account, ctx).await {
        Ok((answers, usage)) => {
            out.answers = answers;
            out.usage = usage;
        }
        Err(e) => {
            out.stop_reason = "error";
            out.error_message = Some(e);
        }
    }
    out
}

type Answers = (BTreeMap<String, Answer>, Option<Value>);

async fn run(
    http: &reqwest::Client,
    m: &Model,
    key: &str,
    account: Option<&str>,
    ctx: &Context,
) -> Result<Answers, String> {
    if !ctx.images.is_empty() && !m.images {
        return Err(format!("{} does not take images", m.id));
    }
    let (url, body) = match m.wire {
        Wire::SystemOne => {
            let mut b = system_one(ctx);
            b.insert("model".into(), json!(m.id));
            (format!("{}/systemone", m.base_url), Value::Object(b))
        }
        Wire::Cloudflare => {
            let account = account.ok_or("Cloudflare needs CLOUDFLARE_ACCOUNT_ID")?;
            (
                format!("{}/run", m.base_url.replace("{account}", account)),
                json!({ "model": m.id, "input": system_one(ctx) }),
            )
        }
        Wire::Decisions => (format!("{}/decisions", m.base_url), decisions(m, ctx)),
    };
    let resp = http
        .post(&url)
        .timeout(Duration::from_secs(120))
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    if !status.is_success() || v["success"] == false {
        let msg = v["error"]["message"]
            .as_str()
            .or(v["errors"][0]["message"].as_str())
            .map(String::from)
            .unwrap_or_else(|| text.chars().take(300).collect());
        return Err(format!("HTTP {}: {msg}", status.as_u16()));
    }
    let r = match m.wire {
        Wire::SystemOne => v,
        // Hosted models answer directly; third-party ones wrap a run record.
        Wire::Cloudflare => match &v["result"] {
            r if r.get("answers").is_some() => r.clone(),
            r if r["state"] == "Completed" => r["result"].clone(),
            r => return Err(format!("the run did not complete (state {})", r["state"])),
        },
        Wire::Decisions => v,
    };
    let usage = usage(&r["usage"], m);
    let answers = match m.wire {
        Wire::Decisions => parse_decisions(&r["answers"], ctx)?,
        _ => parse_system_one(&r["answers"], ctx)?,
    };
    Ok((answers, usage))
}

/// System One request: `bool` questions are called `noul` on the wire.
fn system_one(ctx: &Context) -> Map<String, Value> {
    let questions: Map<String, Value> = ctx
        .questions
        .iter()
        .map(|(id, q)| {
            let v = match q {
                Question::Choice { instructions, criteria } => {
                    json!({ "type": "choice", "instructions": instructions, "criteria": criteria })
                }
                Question::Score { instructions, criteria } => {
                    json!({ "type": "score", "instructions": instructions, "criteria": criteria })
                }
                Question::Bool { instructions, criteria } => {
                    json!({ "type": "noul", "instructions": instructions, "criteria": criteria })
                }
            };
            (id.clone(), v)
        })
        .collect();
    let mut o = Map::new();
    o.insert("state".into(), Value::Object(ctx.state.clone()));
    o.insert("questions".into(), Value::Object(questions));
    o
}

fn decisions(m: &Model, ctx: &Context) -> Value {
    let state = Value::Object(ctx.state.clone()).to_string();
    let input =
        if ctx.images.is_empty() {
            json!(state)
        } else {
            let mut content = vec![json!({ "type": "input_text", "text": state })];
            content.extend(ctx.images.iter().map(
                |i| json!({ "type": "input_image", "image_url": format!("data:{};base64,{}", i.mime_type, i.data) }),
            ));
            json!([{ "role": "user", "content": content }])
        };
    let questions: Vec<Value> = ctx
        .questions
        .iter()
        .map(|(name, q)| match q {
            Question::Choice { instructions, criteria } => json!({
                "type": "choice", "name": name, "instructions": instructions,
                "choices": criteria.iter().map(|(v, d)| if d.is_empty() { json!({ "value": v }) } else { json!({ "value": v, "description": d }) }).collect::<Vec<_>>(),
            }),
            Question::Score { instructions, criteria } => json!({
                "type": "score", "name": name, "instructions": instructions,
                "levels": criteria.iter().map(|l| json!({ "label": l })).collect::<Vec<_>>(),
            }),
            Question::Bool { instructions, criteria } => {
                let mut text = instructions.clone();
                let meanings: Vec<String> = [("True", &criteria.r#true), ("False", &criteria.r#false)]
                    .iter()
                    .filter(|(_, c)| !c.is_empty())
                    .map(|(k, c)| format!("{k} means: {c}"))
                    .collect();
                if !meanings.is_empty() {
                    text = format!("{text}\n\n{}", meanings.join("\n"));
                }
                json!({ "type": "predicate", "name": name, "instructions": text })
            }
        })
        .collect();
    json!({ "model": m.id, "input": input, "questions": questions })
}

fn num(v: &Value, what: &str) -> Result<f64, String> {
    v.as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("invalid {what} in the answer"))
}

fn parse_system_one(v: &Value, ctx: &Context) -> Result<BTreeMap<String, Answer>, String> {
    ctx.questions
        .iter()
        .map(|(id, q)| {
            let a = v.get(id).ok_or_else(|| format!("no answer for {id}"))?;
            let ans = match q {
                Question::Choice { .. } => Answer::Choice {
                    choice: a["choice"]
                        .as_str()
                        .ok_or_else(|| format!("no choice for {id}"))?
                        .into(),
                    probabilities: probs(&a["probabilities"], id)?,
                    confidence: num(&a["confidence"], "confidence")?,
                },
                Question::Score { .. } => Answer::Score {
                    score: num(&a["score"], "score")?,
                    confidence: num(&a["confidence"], "confidence")?,
                },
                Question::Bool { .. } => Answer::Bool {
                    probability: num(&a["noul"], "probability")?,
                },
            };
            Ok((id.clone(), ans))
        })
        .collect()
}

fn probs(v: &Value, id: &str) -> Result<BTreeMap<String, f64>, String> {
    match v {
        Value::Object(o) => o.iter().map(|(k, p)| Ok((k.clone(), num(p, "probability")?))).collect(),
        // Decisions: [{value, probability}].
        Value::Array(a) => a
            .iter()
            .map(|e| {
                Ok((
                    e["value"].as_str().ok_or("bad probability entry")?.to_string(),
                    num(&e["probability"], "probability")?,
                ))
            })
            .collect(),
        _ => Err(format!("invalid probabilities for {id}")),
    }
}

fn parse_decisions(v: &Value, ctx: &Context) -> Result<BTreeMap<String, Answer>, String> {
    let list = v.as_array().ok_or("unexpected answer format")?;
    ctx.questions
        .iter()
        .map(|(id, q)| {
            let a = list
                .iter()
                .find(|a| a["name"] == id.as_str())
                .ok_or_else(|| format!("no answer for {id}"))?;
            if a["type"] == "refusal" {
                return Err(format!("the model refused to answer {id}"));
            }
            let ans = match q {
                Question::Choice { .. } => Answer::Choice {
                    choice: a["choice"]
                        .as_str()
                        .ok_or_else(|| format!("no choice for {id}"))?
                        .into(),
                    probabilities: probs(&a["probabilities"], id)?,
                    confidence: num(&a["confidence"], "confidence")?,
                },
                Question::Score { .. } => Answer::Score {
                    score: num(&a["score"], "score")?,
                    confidence: num(&a["confidence"], "confidence")?,
                },
                Question::Bool { .. } => Answer::Bool {
                    probability: num(&a["probability"], "probability")?,
                },
            };
            Ok((id.clone(), ans))
        })
        .collect()
}

fn usage(u: &Value, m: &Model) -> Option<Value> {
    let input = u["input_tokens"].as_u64();
    let output = u["output_tokens"].as_u64();
    if input.is_none() && output.is_none() {
        return None;
    }
    let (i, o) = (input.unwrap_or(0), output.unwrap_or(0));
    Some(json!({ "input": i, "output": o, "totalTokens": i + o, "cost": { "total": i as f64 * m.cost_input / 1e6 } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        serde_json::from_value(json!({
            "state": {"message": "works, thanks"},
            "questions": {
                "mood": {"type": "choice", "instructions": "Mood?", "criteria": {"happy": "glad", "sad": "unhappy"}},
                "urgent": {"type": "bool", "instructions": "Urgent?", "criteria": {"true": "now", "false": "later"}},
                "level": {"type": "score", "instructions": "Level", "criteria": ["low", "high"]}
            }
        }))
        .unwrap()
    }

    #[test]
    fn wire_formats_round_trip() {
        let c = ctx();
        let s1 = Value::Object(system_one(&c));
        assert_eq!(s1["questions"]["urgent"]["type"], "noul");
        let a = parse_system_one(
            &json!({
                "mood": {"type": "choice", "choice": "happy", "probabilities": {"happy": 0.9, "sad": 0.1}, "confidence": 0.8},
                "urgent": {"type": "noul", "noul": 0.2},
                "level": {"type": "score", "score": 0.3, "confidence": 0.7}
            }),
            &c,
        )
        .unwrap();
        assert_eq!(a["urgent"], Answer::Bool { probability: 0.2 });
        let d = decisions(find("openai/gpt-6-luna").unwrap(), &c);
        assert_eq!(d["questions"][2]["type"], "predicate");
        assert!(
            d["questions"][2]["instructions"]
                .as_str()
                .unwrap()
                .contains("True means: now")
        );
        let a = parse_decisions(
            &json!([
                {"name": "mood", "type": "choice", "choice": "sad", "probabilities": [{"value": "sad", "probability": 0.6}], "confidence": 0.5},
                {"name": "urgent", "type": "predicate", "probability": 0.9},
                {"name": "level", "type": "score", "score": 1.0, "confidence": 0.9}
            ]),
            &c,
        )
        .unwrap();
        assert!(matches!(&a["mood"], Answer::Choice { choice, .. } if choice == "sad"));
        assert!(parse_system_one(&json!({}), &c).is_err());
        assert!(find("cloudflare-workers-ai/@cf/cloudflare/clef").is_some());
        assert_eq!(
            find("typesafe/jev-1.13").unwrap().provider,
            "openrouter",
            "bare id, one owner"
        );
        assert!(find("nope").is_none());
        let q: Question =
            serde_json::from_value(json!({"type": "choice", "instructions": "x", "criteria": ["a", "b"]})).unwrap();
        assert!(matches!(q, Question::Choice { criteria, .. } if criteria.len() == 2));
    }
}
