//! Settings (L0.6): global + project JSONC, deep-merged (project wins).
//! Loading never fails: problems become diagnostics and the rest still loads.
//! Writes edit one key in place, keeping the user's comments and layout.

use crate::storage::{self, Durability};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Renamed keys, applied on load: (old dotted key, new dotted key).
const MIGRATIONS: &[(&str, &str)] = &[];

/// Top-level keys a project file may not set (L0.6).
/// shortcut: names fixed with the L6 work in M3; add keys there as they appear.
const GLOBAL_ONLY: &[&str] = &["permissions", "trust"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: Option<PathBuf>,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct Settings {
    merged: Value,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl Settings {
    /// Load `global`, then `project` on top when the project is trusted.
    pub fn load(global: &Path, project: Option<&Path>, project_trusted: bool) -> Self {
        let mut diags = Vec::new();
        let mut merged = read_scope(global, &mut diags);
        if let Some(project) = project {
            match (project.exists(), project_trusted) {
                (false, _) => {}
                (true, false) => diags.push(diag(project, "ignored: project is not trusted")),
                (true, true) => {
                    let mut p = read_scope(project, &mut diags);
                    for key in GLOBAL_ONLY {
                        if p.as_object_mut().and_then(|o| o.remove(*key)).is_some() {
                            diags.push(diag(
                                project,
                                &format!("`{key}` is only allowed in global settings; ignored"),
                            ));
                        }
                    }
                    deep_merge(&mut merged, p);
                }
            }
        }
        for d in &diags {
            tracing::warn!(file = ?d.file, "settings: {}", d.message);
        }
        Self {
            merged,
            diagnostics: Mutex::new(diags),
        }
    }

    /// Typed value at a dotted key. A wrong-typed value is reported and
    /// treated as unset, so one bad field never resets the others.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        // Keeps docs/config.md complete: every key read must be in the reference table.
        debug_assert!(
            crate::settings_doc::documented(key),
            "settings key `{key}` is not in settings_doc::KEYS"
        );
        let v = lookup(&self.merged, key)?;
        match T::deserialize(v) {
            Ok(t) => Some(t),
            Err(e) => {
                let message = format!("`{key}`: {e}; using default");
                tracing::warn!("settings: {message}");
                let mut d = self.diagnostics.lock().unwrap_or_else(|p| p.into_inner());
                if !d.iter().any(|x| x.message == message) {
                    d.push(Diagnostic { file: None, message });
                }
                None
            }
        }
    }

    /// Unknown keys, wrong types and bad values across the merged settings (`doctor`).
    pub fn check(&self) -> Vec<Diagnostic> {
        crate::settings_doc::check(&self.merged)
            .into_iter()
            .map(|message| Diagnostic { file: None, message })
            .collect()
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

fn diag(file: &Path, message: &str) -> Diagnostic {
    Diagnostic {
        file: Some(file.to_path_buf()),
        message: message.to_string(),
    }
}

fn read_scope(path: &Path, diags: &mut Vec<Diagnostic>) -> Value {
    let empty = Value::Object(Map::new());
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return empty,
        Err(e) => {
            diags.push(diag(path, &format!("cannot read: {e}")));
            return empty;
        }
    };
    let mut value = match jsonc_parser::parse_to_serde_value::<Value>(&text, &ParseOptions::default()) {
        Ok(Value::Object(o)) => Value::Object(o),
        Ok(Value::Null) => return empty, // empty file
        Ok(_) => {
            diags.push(diag(path, "top level must be an object; file ignored"));
            return empty;
        }
        Err(e) => {
            diags.push(diag(path, &format!("parse error, file ignored: {e}")));
            return empty;
        }
    };
    migrate(&mut value, MIGRATIONS, path, diags);
    value
}

fn migrate(value: &mut Value, renames: &[(&str, &str)], file: &Path, diags: &mut Vec<Diagnostic>) {
    for (old, new) in renames {
        let Some(v) = take(value, old) else { continue };
        if lookup(value, new).is_none() {
            insert(value, new, v);
        }
        diags.push(diag(file, &format!("`{old}` was renamed to `{new}`")));
    }
}

/// Objects merge key by key; anything else in `over` replaces `base`.
fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

fn lookup<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(v, |v, k| v.as_object()?.get(k))
}

fn take(v: &mut Value, key: &str) -> Option<Value> {
    let (parent, last) = match key.rsplit_once('.') {
        Some((p, l)) => (p.split('.').try_fold(v, |v, k| v.as_object_mut()?.get_mut(k))?, l),
        None => (v, key),
    };
    parent.as_object_mut()?.remove(last)
}

fn insert(v: &mut Value, key: &str, new: Value) {
    let mut cur = v;
    let parts: Vec<&str> = key.split('.').collect();
    for k in &parts[..parts.len() - 1] {
        let obj = cur.as_object_mut().expect("insert into object");
        let next = obj.entry(*k).or_insert_with(|| Value::Object(Map::new()));
        if !next.is_object() {
            *next = Value::Object(Map::new());
        }
        cur = next;
    }
    cur.as_object_mut()
        .expect("insert into object")
        .insert(parts[parts.len() - 1].to_string(), new);
}

/// Set one dotted key in a settings file, keeping comments and formatting.
/// Refuses to touch a file that doesn't parse, so a hand-edit isn't lost.
pub fn set(path: &Path, key: &str, value: &Value) -> io::Result<()> {
    let _lock = storage::lock(path)?;
    let text = match std::fs::read_to_string(path) {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => "{}\n".to_string(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => "{}\n".to_string(),
        Err(e) => return Err(e),
    };
    let root = CstRootNode::parse(&text, &ParseOptions::default())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
    let mut obj: CstObject = root.object_value_or_set();
    let parts: Vec<&str> = key.split('.').collect();
    for k in &parts[..parts.len() - 1] {
        obj = obj.object_value_or_set(k);
    }
    let last = parts[parts.len() - 1];
    match obj.get(last) {
        Some(prop) => prop.set_value(to_cst(value)),
        None => {
            obj.append(last, to_cst(value));
        }
    }
    storage::write_atomic(path, root.to_string().as_bytes(), Durability::Durable)
}

fn to_cst(v: &Value) -> CstInputValue {
    match v {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(a) => CstInputValue::Array(a.iter().map(to_cst).collect()),
        Value::Object(o) => CstInputValue::Object(o.iter().map(|(k, v)| (k.clone(), to_cst(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn files() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let (g, p) = (d.path().join("global.json"), d.path().join("project.json"));
        (d, g, p)
    }

    #[test]
    fn jsonc_deep_merge_project_wins() {
        let (_d, g, p) = files();
        fs::write(
            &g,
            "// mine\n{ \"ui\": { \"theme\": \"dark\", \"turnCollapse\": true, }, \"model\": \"a\" }",
        )
        .unwrap();
        fs::write(&p, "{ /* repo */ \"ui\": { \"theme\": \"light\" } }").unwrap();
        let s = Settings::load(&g, Some(&p), true);
        assert_eq!(s.get::<String>("ui.theme").as_deref(), Some("light"));
        assert_eq!(s.get::<bool>("ui.turnCollapse"), Some(true));
        assert_eq!(s.get::<String>("model").as_deref(), Some("a"));
        assert!(s.diagnostics().is_empty(), "{:?}", s.diagnostics());
    }

    #[test]
    fn untrusted_project_and_global_only_keys_are_ignored() {
        let (_d, g, p) = files();
        fs::write(&p, r#"{ "model": "p", "permissions": { "mode": "auto" } }"#).unwrap();
        let s = Settings::load(&g, Some(&p), false);
        assert_eq!(s.get::<String>("model"), None);
        assert!(s.diagnostics()[0].message.contains("not trusted"));
        let s = Settings::load(&g, Some(&p), true);
        assert_eq!(s.get::<String>("model").as_deref(), Some("p"));
        assert_eq!(s.get::<Value>("permissions"), None);
        assert!(s.diagnostics()[0].message.contains("only allowed in global"));
    }

    #[test]
    fn bad_field_does_not_reset_others() {
        let (_d, g, _p) = files();
        fs::write(&g, r#"{ "maxTokens": "lots", "model": "a" }"#).unwrap();
        let s = Settings::load(&g, None, false);
        assert_eq!(s.get::<u32>("maxTokens").unwrap_or(3), 3);
        assert_eq!(s.get::<String>("model").as_deref(), Some("a"));
        assert_eq!(s.diagnostics().len(), 1);
    }

    #[test]
    fn broken_file_is_a_diagnostic_not_a_crash() {
        let (_d, g, p) = files();
        fs::write(&g, "{ \"model\": ").unwrap();
        fs::write(&p, "[1]").unwrap();
        let s = Settings::load(&g, Some(&p), true);
        assert_eq!(s.get::<String>("model"), None);
        assert_eq!(s.diagnostics().len(), 2);
    }

    #[test]
    fn migration_renames_key() {
        let mut v = json!({ "queueMode": "steer", "retry": { "maxDelayMs": 5 } });
        let mut d = Vec::new();
        let renames = [
            ("queueMode", "steeringMode"),
            ("retry.maxDelayMs", "retry.provider.maxRetryDelayMs"),
        ];
        migrate(&mut v, &renames, Path::new("x"), &mut d);
        assert_eq!(
            v,
            json!({ "steeringMode": "steer", "retry": { "provider": { "maxRetryDelayMs": 5 } } })
        );
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn set_keeps_comments_and_creates_nested_keys() {
        let (_d, g, _p) = files();
        fs::write(&g, "{\n  // my theme\n  \"ui\": { \"theme\": \"dark\" },\n}\n").unwrap();
        set(&g, "ui.theme", &json!("light")).unwrap();
        set(&g, "ui.panel.host", &json!("tmux")).unwrap();
        let text = fs::read_to_string(&g).unwrap();
        assert!(text.contains("// my theme"), "{text}");
        let s = Settings::load(&g, None, false);
        assert_eq!(s.get::<String>("ui.theme").as_deref(), Some("light"));
        assert_eq!(s.get::<String>("ui.panel.host").as_deref(), Some("tmux"));
        set(&_d.path().join("new/settings.json"), "a", &json!(1)).unwrap();
    }

    #[test]
    fn set_refuses_unparseable_file() {
        let (_d, g, _p) = files();
        fs::write(&g, "{ broken").unwrap();
        assert!(set(&g, "a", &json!(1)).is_err());
        assert_eq!(fs::read_to_string(&g).unwrap(), "{ broken");
    }
}
