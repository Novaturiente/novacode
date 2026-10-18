//! Tool-schema dialects (L2.20). Some backends accept only a subset of JSON
//! Schema and reject the whole request over one keyword.
//! Layer 1, prevention: rewrite into a known subset before sending.
//! Layer 2, recovery: read the rejection, strip what was named, retry.
//! Layer 3, memory: remember it per provider/model on disk, so it's learned once.

use novacode_base::storage::{self, Durability};
use novacode_types::ToolDef;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// Keywords the OpenAPI-3.0 subset (Google's `Schema` proto) accepts.
const OPENAPI: &[&str] = &[
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "enum",
    "maxItems",
    "minItems",
    "properties",
    "required",
    "minProperties",
    "maxProperties",
    "minLength",
    "maxLength",
    "pattern",
    "example",
    "anyOf",
    "propertyOrdering",
    "default",
    "items",
    "minimum",
    "maximum",
];

/// Rewrite a JSON Schema into the OpenAPI subset: local `$ref`s inlined,
/// `type: [T, "null"]` → `type: T, nullable: true`, `const` → `enum`,
/// `oneOf` → `anyOf`, and every other keyword dropped.
pub fn openapi_subset(schema: &Value) -> Value {
    let defs = schema
        .get("$defs")
        .or_else(|| schema.get("definitions"))
        .cloned()
        .unwrap_or(Value::Null);
    convert(schema, &defs, 0)
}

fn convert(v: &Value, defs: &Value, depth: usize) -> Value {
    let Some(o) = v.as_object() else { return v.clone() };
    if depth > 16 {
        return json!({ "type": "object" });
    }
    // Inline a local reference (siblings like `description` win over the target's).
    if let Some(r) = o.get("$ref").and_then(Value::as_str) {
        let name = r.rsplit('/').next().unwrap_or_default();
        let mut target = defs.get(name).cloned().unwrap_or_else(|| json!({ "type": "object" }));
        if let (Some(t), true) = (target.as_object_mut(), o.len() > 1) {
            for (k, x) in o.iter().filter(|(k, _)| *k != "$ref") {
                t.insert(k.clone(), x.clone());
            }
        }
        return convert(&target, defs, depth + 1);
    }
    let mut out = Map::new();
    for (k, x) in o {
        match k.as_str() {
            "type" => match x {
                Value::Array(ts) => {
                    let real: Vec<&Value> = ts.iter().filter(|t| *t != "null").collect();
                    if real.len() < ts.len() {
                        out.insert("nullable".into(), json!(true));
                    }
                    match real.as_slice() {
                        [one] => {
                            out.insert("type".into(), (*one).clone());
                        }
                        [] => {}
                        many => {
                            let any: Vec<Value> = many.iter().map(|t| json!({ "type": t })).collect();
                            out.insert("anyOf".into(), Value::Array(any));
                        }
                    }
                }
                t => {
                    out.insert("type".into(), t.clone());
                }
            },
            "const" => {
                out.insert("enum".into(), json!([x]));
            }
            "properties" => {
                let props: Map<String, Value> = x
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(name, s)| (name.clone(), convert(s, defs, depth + 1)))
                    .collect();
                out.insert("properties".into(), Value::Object(props));
            }
            "items" => {
                out.insert("items".into(), convert(x, defs, depth + 1));
            }
            "anyOf" | "oneOf" => {
                let mut alts: Vec<Value> = Vec::new();
                for s in x.as_array().into_iter().flatten() {
                    // `{type: null}` alternatives become `nullable`.
                    if s.get("type") == Some(&json!("null")) {
                        out.insert("nullable".into(), json!(true));
                    } else {
                        alts.push(convert(s, defs, depth + 1));
                    }
                }
                match alts.len() {
                    0 => {}
                    // A single alternative is just that schema.
                    1 => {
                        if let Some(one) = alts.pop().and_then(|a| a.as_object().cloned()) {
                            for (k2, v2) in one {
                                out.entry(k2).or_insert(v2);
                            }
                        }
                    }
                    _ => {
                        out.insert("anyOf".into(), Value::Array(alts));
                    }
                }
            }
            k if OPENAPI.contains(&k) => {
                out.insert(k.into(), x.clone());
            }
            _ => {} // $schema, additionalProperties, examples, $defs, …
        }
    }
    // `enum` values must be strings in the proto.
    if let Some(Value::Array(e)) = out.get_mut("enum") {
        for x in e.iter_mut() {
            if !x.is_string() {
                *x = json!(x.to_string());
            }
        }
    }
    Value::Object(out)
}

/// What a provider's 400 said it can't take.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub keywords: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub formats: BTreeSet<String>,
}

/// Parse a schema rejection out of an error message (jcode's real cases).
/// `None` when the error isn't one that stripping a keyword can fix.
pub fn rejected(msg: &str) -> Option<Rejection> {
    let mut r = Rejection::default();
    // Gemini/Antigravity: Unknown name "propertyNames" at '…function_declarations…': Cannot find field.
    if msg.contains("Cannot find field") {
        r.keywords.extend(quoted_after(msg, "Unknown name"));
    }
    // OpenAI: 'uri' is not a valid format. / 'uniqueItems' is not permitted.
    r.formats.extend(quoted_before(msg, "is not a valid format"));
    r.keywords.extend(quoted_before(msg, "is not permitted"));
    // Anthropic-style bridges: top-level combiners.
    if msg.contains("does not support oneOf, allOf, or anyOf") {
        r.keywords.extend(["oneOf", "allOf", "anyOf"].map(String::from));
    }
    // Never strip structure the schema can't work without.
    r.keywords
        .retain(|k| !matches!(k.as_str(), "type" | "properties" | "items" | "required"));
    (!r.keywords.is_empty() || !r.formats.is_empty()).then_some(r)
}

/// Each `"X"` / `\"X\"` after `marker` (a Gemini 400 lists one per violation).
fn quoted_after(msg: &str, marker: &str) -> Vec<String> {
    msg.match_indices(marker)
        .filter_map(|(i, _)| {
            let rest = msg[i + marker.len()..].trim_start().trim_start_matches('\\');
            let rest = rest.strip_prefix('"')?;
            let end = rest.find(['"', '\\'])?;
            Some(rest[..end].to_string())
        })
        .filter(|k| !k.is_empty())
        .collect()
}

/// Each `'X'` right before `marker`.
fn quoted_before(msg: &str, marker: &str) -> Vec<String> {
    msg.match_indices(marker)
        .filter_map(|(i, _)| {
            let head = msg[..i].trim_end().strip_suffix('\'')?;
            let start = head.rfind('\'')?;
            Some(head[start + 1..].to_string())
        })
        .filter(|k| !k.is_empty())
        .collect()
}

/// Remove rejected keywords (and `format` values) everywhere in a schema.
/// Property names are data, not keywords, so they're never stripped.
pub fn strip(v: &Value, r: &Rejection) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, x)| {
                    !r.keywords.contains(*k) && !(*k == "format" && x.as_str().is_some_and(|f| r.formats.contains(f)))
                })
                .map(|(k, x)| {
                    let x = if k == "properties" {
                        let props = x.as_object().into_iter().flatten();
                        Value::Object(props.map(|(n, s)| (n.clone(), strip(s, r))).collect())
                    } else {
                        strip(x, r)
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| strip(x, r)).collect()),
        other => other.clone(),
    }
}

/// Learned rejections per `provider/model`, kept in `<home>/cache/schema-quirks.json`.
/// Advisory: an unreadable file means learning again, never a failure.
pub struct Quirks {
    path: Option<PathBuf>,
    map: Mutex<BTreeMap<String, Rejection>>,
}

impl Quirks {
    pub fn load(home: &std::path::Path) -> Self {
        let path = home.join("cache").join("schema-quirks.json");
        let map = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            map: Mutex::new(map),
        }
    }

    /// In memory only (tests).
    pub fn ephemeral() -> Self {
        Self {
            path: None,
            map: Mutex::default(),
        }
    }

    /// The tool defs with this route's learned rejections stripped.
    pub fn apply(&self, route: &str, defs: &[ToolDef]) -> Vec<ToolDef> {
        let map = self.map.lock().unwrap();
        let Some(r) = map.get(route) else { return defs.to_vec() };
        defs.iter()
            .map(|d| ToolDef {
                schema: strip(&d.schema, r),
                ..d.clone()
            })
            .collect()
    }

    /// Record a rejection. True if it taught something new (so a retry can help).
    pub fn learn(&self, route: &str, r: &Rejection) -> bool {
        let snapshot = {
            let mut map = self.map.lock().unwrap();
            let e = map.entry(route.into()).or_default();
            let before = (e.keywords.len(), e.formats.len());
            e.keywords.extend(r.keywords.iter().cloned());
            e.formats.extend(r.formats.iter().cloned());
            if before == (e.keywords.len(), e.formats.len()) {
                return false;
            }
            serde_json::to_vec_pretty(&*map).unwrap_or_default()
        };
        if let Some(p) = &self.path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = storage::write_atomic(p, &snapshot, Durability::Fast) {
                tracing::warn!(error = %e, "could not save schema quirks");
            }
        }
        tracing::info!(route, keywords = ?r.keywords, formats = ?r.formats, "learned schema quirk");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_rejections() {
        let g = r#"Invalid JSON payload received. Unknown name \"dependentRequired\" at 'request.tools[0].function_declarations[14].parameters.properties[3].value': Cannot find field. Invalid JSON payload received. Unknown name \"unevaluatedItems\" at 'x': Cannot find field."#;
        let r = rejected(g).unwrap();
        assert_eq!(
            r.keywords,
            ["dependentRequired", "unevaluatedItems"].map(String::from).into()
        );
        let o = "Invalid schema for function 'mcp__x__y': In context=('properties', 'ids'), 'uniqueItems' is not permitted.";
        assert_eq!(rejected(o).unwrap().keywords, ["uniqueItems".to_string()].into());
        let f = "Invalid schema for function 'm': In context=('properties', 'url'), 'uri' is not a valid format.";
        assert_eq!(rejected(f).unwrap().formats, ["uri".to_string()].into());
        // Structural or unrelated errors aren't actionable.
        assert!(rejected("Unknown name \"type\" at 'x': Proto field is not repeating, cannot start list.").is_none());
        assert!(rejected("In context=(), schema must have a 'type' key.").is_none());
        assert!(rejected("The requested model is not supported.").is_none());
    }

    #[test]
    fn strips_and_remembers() {
        let s = json!({ "type": "object", "properties": {
            "uniqueItems": { "type": "array", "uniqueItems": true, "items": { "type": "string", "format": "uri" } },
            "d": { "type": "string", "format": "date" }
        }});
        let r = Rejection {
            keywords: ["uniqueItems".to_string()].into(),
            formats: ["uri".to_string()].into(),
        };
        let o = strip(&s, &r);
        let p = &o["properties"];
        assert_eq!(
            p["uniqueItems"],
            json!({ "type": "array", "items": { "type": "string" } }),
            "property name kept"
        );
        assert_eq!(p["d"]["format"], "date");
        let dir = tempfile::tempdir().unwrap();
        let q = Quirks::load(dir.path());
        assert!(q.learn("p/m", &r) && !q.learn("p/m", &r), "second time teaches nothing");
        let defs = vec![ToolDef {
            name: "t".into(),
            description: String::new(),
            schema: s,
        }];
        assert_eq!(Quirks::load(dir.path()).apply("p/m", &defs)[0].schema, o, "persisted");
        assert_eq!(q.apply("p/other", &defs)[0].schema, defs[0].schema, "per route");
    }

    #[test]
    fn openapi_subset_rewrites() {
        let s = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": { "type": "string", "description": "file" },
                "limit": { "type": ["integer", "null"], "format": "uint", "minimum": 0 },
                "mode": { "$ref": "#/$defs/Mode", "description": "how" },
                "level": { "anyOf": [{ "$ref": "#/$defs/Mode" }, { "type": "null" }] },
                "tags": { "type": "array", "items": { "type": ["string", "number"] } },
                "kind": { "const": "x" }
            },
            "required": ["path"],
            "$defs": { "Mode": { "type": "string", "enum": ["a", "b"] } }
        });
        let o = openapi_subset(&s);
        assert!(o.get("$schema").is_none() && o.get("additionalProperties").is_none() && o.get("$defs").is_none());
        let p = &o["properties"];
        assert_eq!(
            p["limit"],
            json!({ "type": "integer", "nullable": true, "format": "uint", "minimum": 0 })
        );
        assert_eq!(
            p["mode"],
            json!({ "type": "string", "enum": ["a", "b"], "description": "how" })
        );
        assert_eq!(
            p["level"],
            json!({ "nullable": true, "type": "string", "enum": ["a", "b"] })
        );
        assert_eq!(
            p["tags"]["items"],
            json!({ "anyOf": [{ "type": "string" }, { "type": "number" }] })
        );
        assert_eq!(p["kind"], json!({ "enum": ["x"] }));
        assert_eq!(o["required"], json!(["path"]));
    }
}
