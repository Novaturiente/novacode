//! Tool-schema dialects (L2.20), layer 1: prevention. Some backends accept
//! only a subset of JSON Schema and reject the whole request over one
//! keyword, so schemas are rewritten into that subset before sending.

use serde_json::{Map, Value, json};

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

#[cfg(test)]
mod tests {
    use super::*;

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
