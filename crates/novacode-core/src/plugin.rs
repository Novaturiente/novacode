//! Claude Code plugins as packages (L7.14). A package is read one of two ways:
//!
//! - **novacode package**: `extensions/`, `skills/` (searched to any depth),
//!   `prompts/`, `agents/`, `themes/`, `mcp.json`.
//! - **Claude Code plugin**: `.claude-plugin/plugin.json`, or (without a
//!   `novacode-package.json`) any of `commands/`, `hooks/hooks.json`, `.mcp.json`.
//!   Its parts follow Claude Code's rules: `skills/<name>/SKILL.md` one level deep
//!   plus the manifest's `skills` folders, `commands/` as prompt templates,
//!   `agents/`, `.mcp.json` plus the manifest's `mcpServers`, and hooks
//!   (`hooks/hooks.json` plus the manifest's `hooks`, run by [`crate::plugin_hooks`]).
//!
//! `${CLAUDE_PLUGIN_ROOT}`, `${CLAUDE_PLUGIN_DATA}` and `${user_config.KEY}` are
//! filled in. Options a plugin declares (`userConfig`) live in
//! `<store>/package-options.json` (owner-only), next to the package folder.

use crate::packages::Package;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Native,
    Claude,
}

/// Hook events novacode runs; others are listed as skipped.
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "SubagentStart",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SessionEnd",
];

/// An `mcpServers` map, or why it can't be read.
pub type Servers = Result<Map<String, Value>, String>;

/// One command hook of a plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Hook {
    pub event: String,
    /// Regex on the tool name (tool events), the start source (`SessionStart`) or
    /// the agent type (`SubagentStart`); empty or `*` matches all.
    pub matcher: String,
    pub command: String,
    /// Exec form: run `command` with these arguments, no shell.
    pub args: Option<Vec<String>>,
    /// `bash` instead of `sh` for the shell form.
    pub bash: bool,
    pub timeout_secs: Option<u64>,
    /// `async: true`: started, never waited for.
    pub background: bool,
    /// `once: true`: dropped after its first successful run (skill hooks only, as in Claude Code).
    pub once: bool,
}

/// What one installed package contributes.
#[derive(Debug, Clone)]
pub struct Parts {
    pub kind: Kind,
    pub name: String,
    pub root: PathBuf,
    manifest: Value,
    /// The store folder the package is in (`~/.novacode` or a project's `.novacode`).
    base: PathBuf,
}

impl Parts {
    pub fn of(p: &Package) -> Parts {
        let manifest = read_json(&p.dir.join(".claude-plugin").join("plugin.json"));
        let claude = !manifest.is_null()
            || (!p.dir.join("novacode-package.json").is_file()
                && ["commands", "hooks/hooks.json", ".mcp.json"]
                    .iter()
                    .any(|f| p.dir.join(f).exists()));
        Parts {
            kind: if claude { Kind::Claude } else { Kind::Native },
            name: p.name.clone(),
            root: p.dir.clone(),
            manifest,
            base: p
                .dir
                .parent()
                .and_then(Path::parent)
                .unwrap_or(Path::new(""))
                .to_path_buf(),
        }
    }

    /// The package a resource file belongs to, from its path
    /// (`<base>/packages/<name>/…`, `<base>` a `.novacode` folder or a store with a lockfile).
    pub fn owning(path: &Path) -> Option<Parts> {
        // shortcut: recognised by folder names; pass the owner along with each folder if a custom home breaks it.
        path.ancestors().find_map(|a| {
            let store = a.parent()?;
            let base = store.parent()?;
            let is_store = store.file_name()? == "packages"
                && (base.file_name().is_some_and(|n| n == ".novacode") || base.join("packages.lock.json").is_file());
            if !is_store {
                return None;
            }
            Some(Parts::of(&Package {
                name: a.file_name()?.to_string_lossy().into_owned(),
                dir: a.to_path_buf(),
                link: None,
                disabled: None,
            }))
        })
    }

    /// The manifest's `version`.
    pub fn version(&self) -> Option<String> {
        self.manifest["version"].as_str().map(str::to_string)
    }

    /// `${CLAUDE_PLUGIN_DATA}`: kept across updates, created on first use.
    pub fn data_dir(&self) -> PathBuf {
        let d = self.base.join("plugin-data").join(&self.name);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// A manifest path (`./x`), inside the package or `None`.
    fn path(&self, s: &str) -> Option<PathBuf> {
        let rel = Path::new(s.trim_start_matches("./"));
        (!rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir)))
        .then(|| self.root.join(rel))
    }

    /// The manifest's `key` as paths (a string or a list of strings).
    fn paths(&self, key: &str) -> Option<Vec<PathBuf>> {
        let v = &self.manifest[key];
        let list: Vec<&str> = match v {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => return None,
        };
        Some(list.into_iter().filter_map(|s| self.path(s)).collect())
    }

    /// Folders (or files) of one resource kind: `skills`, `prompts`, `agents`,
    /// `extensions` or `themes`.
    pub fn dirs(&self, kind: &str) -> Vec<PathBuf> {
        if self.kind == Kind::Native {
            return vec![self.root.join(kind)];
        }
        match kind {
            "skills" => {
                let mut roots = vec![self.root.join("skills")];
                roots.extend(self.paths("skills").unwrap_or_default());
                if !roots[0].is_dir() && self.manifest["skills"].is_null() && self.root.join("SKILL.md").is_file() {
                    return vec![self.root.clone()];
                }
                let mut out = Vec::new();
                for r in roots {
                    if r.join("SKILL.md").is_file() {
                        out.push(r);
                    } else if let Ok(rd) = std::fs::read_dir(&r) {
                        let mut kids: Vec<PathBuf> = rd
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| p.join("SKILL.md").is_file())
                            .collect();
                        kids.sort();
                        out.extend(kids);
                    }
                }
                out.dedup();
                out
            }
            "prompts" => match &self.manifest["commands"] {
                // shortcut: a map entry loads its `source` file under the file's name; inline `content` is skipped.
                Value::Object(m) => m
                    .values()
                    .filter_map(|v| v["source"].as_str())
                    .filter_map(|s| self.path(s))
                    .collect(),
                _ => self
                    .paths("commands")
                    .unwrap_or_else(|| vec![self.root.join("commands")]),
            },
            "agents" => self.paths("agents").unwrap_or_else(|| vec![self.root.join("agents")]),
            _ => vec![],
        }
    }

    /// MCP servers: (source file, `mcpServers` map or why it can't be read), in load order.
    pub fn mcp(&self) -> Vec<(PathBuf, Servers)> {
        if self.kind == Kind::Native {
            let f = self.root.join("mcp.json");
            return match std::fs::read_to_string(&f) {
                Ok(t) => vec![(f.clone(), servers_of(&t))],
                Err(_) => vec![],
            };
        }
        let manifest = self.root.join(".claude-plugin").join("plugin.json");
        let mut out = Vec::new();
        let add_file = |f: PathBuf, out: &mut Vec<_>| {
            if let Ok(t) = std::fs::read_to_string(&f) {
                out.push((f, servers_of(&t)));
            }
        };
        add_file(self.root.join(".mcp.json"), &mut out);
        let v = &self.manifest["mcpServers"];
        let items: Vec<&Value> = match v {
            Value::Array(a) => a.iter().collect(),
            Value::Null => vec![],
            v => vec![v],
        };
        for it in items {
            match it {
                // shortcut: `.mcpb`/`.dxt` bundles are skipped; unpack them if a plugin needs one.
                Value::String(s) if s.ends_with(".json") => {
                    if let Some(f) = self.path(s)
                        && f != self.root.join(".mcp.json")
                    {
                        add_file(f, &mut out);
                    }
                }
                Value::Object(m) => out.push((manifest.clone(), Ok(m.clone()))),
                _ => {}
            }
        }
        let opts = self.options();
        for (_, r) in &mut out {
            if let Ok(m) = r {
                for cfg in m.values_mut() {
                    fill_value(cfg, &|s| self.fill_with(s, &opts, true));
                    if cfg.get("command").is_some() {
                        let env = cfg
                            .as_object_mut()
                            .map(|o| o.entry("env").or_insert_with(|| Value::Object(Map::new())));
                        if let Some(Value::Object(env)) = env {
                            for (k, val) in self.env(&opts) {
                                if k != "CLAUDE_PROJECT_DIR" && !k.starts_with("CLAUDE_PLUGIN_OPTION_") {
                                    env.entry(k).or_insert(Value::String(val));
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Hooks novacode runs, and one line per hook it doesn't (unsupported event or type).
    pub fn hooks(&self) -> (Vec<Hook>, Vec<String>) {
        if self.kind == Kind::Native {
            return (vec![], vec![]);
        }
        let mut maps: Vec<Value> = Vec::new();
        let from_file = |f: &Path, maps: &mut Vec<Value>| {
            let v = read_json(f);
            if v["hooks"].is_object() {
                maps.push(v["hooks"].clone());
            }
        };
        from_file(&self.root.join("hooks").join("hooks.json"), &mut maps);
        let items: Vec<Value> = match &self.manifest["hooks"] {
            Value::Array(a) => a.clone(),
            Value::Null => vec![],
            v => vec![v.clone()],
        };
        for it in items {
            match it {
                Value::String(s) => {
                    if let Some(f) = self.path(&s)
                        && f != self.root.join("hooks").join("hooks.json")
                    {
                        from_file(&f, &mut maps);
                    }
                }
                Value::Object(_) => maps.push(it),
                _ => {}
            }
        }
        let (mut out, mut skipped) = (Vec::new(), Vec::new());
        for m in maps {
            parse_hooks(&m, &mut out, &mut skipped);
        }
        skipped.sort();
        skipped.dedup();
        (out, skipped)
    }

    /// The manifest's `userConfig`: (key, spec), in manifest order.
    pub fn option_specs(&self) -> Vec<(String, Value)> {
        self.manifest["userConfig"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn options_file(&self) -> PathBuf {
        self.base.join("package-options.json")
    }

    /// Saved option values over the declared defaults.
    pub fn options(&self) -> BTreeMap<String, Value> {
        let saved = read_json(&self.options_file());
        self.option_specs()
            .into_iter()
            .filter_map(|(k, spec)| {
                let v = saved[&self.name]
                    .get(&k)
                    .cloned()
                    .or_else(|| spec.get("default").cloned())?;
                Some((k, v))
            })
            .collect()
    }

    /// Save option values (owner-only file: it may hold secrets).
    pub fn save_options(&self, values: &BTreeMap<String, Value>) -> Result<(), String> {
        let f = self.options_file();
        let mut all = read_json(&f);
        if !all.is_object() {
            all = Value::Object(Map::new());
        }
        all[&self.name] = serde_json::to_value(values).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?;
        write_private(&f, &text).map_err(|e| format!("{}: {e}", f.display()))
    }

    /// Environment for hooks and MCP servers.
    pub fn env(&self, opts: &BTreeMap<String, Value>) -> Vec<(String, String)> {
        let mut v = vec![
            ("CLAUDE_PLUGIN_ROOT".to_string(), self.root.display().to_string()),
            ("CLAUDE_PLUGIN_DATA".to_string(), self.data_dir().display().to_string()),
        ];
        for (k, val) in opts {
            v.push((format!("CLAUDE_PLUGIN_OPTION_{}", k.to_uppercase()), plain(val)));
        }
        v
    }

    /// Fill the plugin variables in `s`; sensitive options only when `secrets`
    /// (MCP configs and hooks, never text the model reads).
    pub fn fill(&self, s: &str, secrets: bool) -> String {
        if !s.contains("${") {
            return s.to_string();
        }
        self.fill_with(s, &self.options(), secrets)
    }

    fn fill_with(&self, s: &str, opts: &BTreeMap<String, Value>, secrets: bool) -> String {
        if !s.contains("${") {
            return s.to_string();
        }
        let mut out = s
            .replace(
                "${CLAUDE_PLUGIN_ROOT}",
                &novacode_base::platform::slash_path(&self.root),
            )
            .replace(
                "${CLAUDE_PLUGIN_DATA}",
                &novacode_base::platform::slash_path(&self.data_dir()),
            );
        let sensitive: Vec<String> = self
            .option_specs()
            .into_iter()
            .filter(|(_, s)| s["sensitive"] == true)
            .map(|(k, _)| k)
            .collect();
        for (k, v) in opts {
            let val = if secrets || !sensitive.contains(k) {
                plain(v)
            } else {
                format!("<{k}: secret>")
            };
            out = out.replace(&format!("${{user_config.{k}}}"), &val);
        }
        out
    }
}

/// The command hooks of one Claude Code `hooks` map (`{event: [{matcher, hooks}]}`,
/// from `hooks.json` or skill frontmatter); the rest goes to `skipped`.
pub fn parse_hooks(m: &Value, out: &mut Vec<Hook>, skipped: &mut Vec<String>) {
    for (event, groups) in m.as_object().into_iter().flatten() {
        for g in groups.as_array().into_iter().flatten() {
            for h in g["hooks"].as_array().into_iter().flatten() {
                let ty = h["type"].as_str().unwrap_or("command");
                let Some(command) = h["command"].as_str().filter(|_| ty == "command") else {
                    skipped.push(format!("{event} `{ty}` hook (only command hooks run)"));
                    continue;
                };
                if !EVENTS.contains(&event.as_str()) {
                    skipped.push(format!("{event} hook (novacode has no such event)"));
                    continue;
                }
                out.push(Hook {
                    event: event.clone(),
                    matcher: g["matcher"].as_str().unwrap_or_default().to_string(),
                    command: command.to_string(),
                    args: h["args"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()),
                    bash: h["shell"] == "bash",
                    timeout_secs: h["timeout"].as_u64(),
                    background: h["async"] == true,
                    once: h["once"] == true,
                });
            }
        }
    }
}

/// Fill plugin variables in a resource file's text when it belongs to a package.
pub fn fill_for(path: &Path, text: String) -> String {
    if !text.contains("${") {
        return text;
    }
    match Parts::owning(path) {
        Some(p) if p.kind == Kind::Claude => p.fill(&text, false),
        _ => text,
    }
}

/// The package name of a resource file, for `<package>:<name>` on a name clash.
pub fn owner_name(path: &Path) -> Option<String> {
    Parts::owning(path).map(|p| p.name)
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(plain).collect::<Vec<_>>().join(","),
        v => v.to_string(),
    }
}

fn fill_value(v: &mut Value, f: &dyn Fn(&str) -> String) {
    match v {
        Value::String(s) => *s = f(s),
        Value::Array(a) => a.iter_mut().for_each(|x| fill_value(x, f)),
        Value::Object(o) => o.values_mut().for_each(|x| fill_value(x, f)),
        _ => {}
    }
}

/// An MCP file: `{"mcpServers": {…}}`, or (Claude plugins) the map itself.
fn servers_of(text: &str) -> Result<Map<String, Value>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    match v.get("mcpServers").unwrap_or(&v) {
        Value::Object(m) => Ok(m.clone()),
        _ => Err("`mcpServers` is not an object".into()),
    }
}

fn read_json(f: &Path) -> Value {
    std::fs::read_to_string(f)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn write_private(f: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(d) = f.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = f.with_extension("json.tmp");
    let mut o = novacode_base::platform::set_create_mode(
        std::fs::OpenOptions::new().write(true).create(true).truncate(true),
        0o600,
    )
    .open(&tmp)?;
    o.write_all(text.as_bytes())?;
    std::fs::rename(tmp, f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn pkg(base: &Path, name: &str) -> Package {
        Package {
            name: name.into(),
            dir: base.join("packages").join(name),
            link: None,
            disabled: None,
        }
    }

    #[test]
    fn claude_plugin_parts() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join(".novacode");
        let p = pkg(&base, "pony");
        let r = &p.dir;
        write(
            &r.join(".claude-plugin/plugin.json"),
            &json!({
                "name": "pony", "version": "1.2.0",
                "skills": ["./extra/one", "./extra"],
                "hooks": ["./hooks/a.json", {"Stop": [{"hooks": [{"type": "command", "command": "s"}]}]}],
                "mcpServers": {"srv": {"command": "${CLAUDE_PLUGIN_ROOT}/bin/s", "args": ["${user_config.tok}"]}},
                "userConfig": {"tok": {"type": "string", "title": "T", "description": "d", "sensitive": true, "default": "x1"}},
            })
            .to_string(),
        );
        write(&r.join("skills/a/SKILL.md"), "---\ndescription: a\n---\n");
        write(&r.join("skills/group/deep/SKILL.md"), "---\ndescription: deep\n---\n");
        write(&r.join("extra/one/SKILL.md"), "---\ndescription: one\n---\n");
        write(&r.join("extra/two/SKILL.md"), "---\ndescription: two\n---\n");
        write(&r.join("commands/go.md"), "go $ARGUMENTS");
        write(
            &r.join("hooks/hooks.json"),
            &json!({"hooks": {"SessionStart": [{"matcher": "startup", "hooks": [{"type": "command", "command": "start"}]}]}})
                .to_string(),
        );
        write(
            &r.join("hooks/a.json"),
            &json!({"hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"command": "x", "args": ["1"], "timeout": 3}]}],
                "PreCompact": [{"hooks": [{"type": "command", "command": "c"}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "prompt", "prompt": "p"}]}],
            }})
            .to_string(),
        );
        let parts = Parts::of(&p);
        assert_eq!(parts.kind, Kind::Claude);
        assert_eq!(parts.version().as_deref(), Some("1.2.0"));
        let skills = parts.dirs("skills");
        assert_eq!(
            skills,
            vec![r.join("skills/a"), r.join("extra/one"), r.join("extra/two")],
            "one level deep, plus the manifest's folders"
        );
        assert_eq!(parts.dirs("prompts"), vec![r.join("commands")]);
        assert!(parts.dirs("extensions").is_empty());
        let (hooks, skipped) = parts.hooks();
        let names: Vec<&str> = hooks.iter().map(|h| h.event.as_str()).collect();
        assert_eq!(names, ["SessionStart", "PreToolUse", "Stop"]);
        assert_eq!(hooks[1].args.as_deref(), Some(&["1".to_string()][..]));
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        let mcp = parts.mcp();
        let srv = &mcp[0].1.as_ref().unwrap()["srv"];
        assert_eq!(
            srv["command"],
            format!("{}/bin/s", novacode_base::platform::slash_path(r))
        );
        assert_eq!(srv["args"][0], "x1", "secrets fill MCP configs");
        assert!(srv["env"]["CLAUDE_PLUGIN_ROOT"].is_string());
        // Text the model reads never gets a secret.
        assert_eq!(parts.fill("t=${user_config.tok}", false), "t=<tok: secret>");
        let mut o = BTreeMap::new();
        o.insert("tok".to_string(), json!("saved"));
        parts.save_options(&o).unwrap();
        assert_eq!(parts.options()["tok"], "saved");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(base.join("package-options.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let owned = Parts::owning(&r.join("skills/a/SKILL.md")).unwrap();
        assert_eq!(owned.name, "pony");
        assert!(owner_name(&t.path().join("elsewhere/SKILL.md")).is_none());
    }

    #[test]
    fn single_skill_and_native() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join(".novacode");
        let one = pkg(&base, "one");
        write(&one.dir.join(".claude-plugin/plugin.json"), r#"{"name":"one"}"#);
        write(&one.dir.join("SKILL.md"), "---\ndescription: d\n---\n");
        assert_eq!(Parts::of(&one).dirs("skills"), vec![one.dir.clone()]);
        let nat = pkg(&base, "nat");
        write(&nat.dir.join("skills/x/y/SKILL.md"), "---\ndescription: d\n---\n");
        write(&nat.dir.join("mcp.json"), r#"{"mcpServers":{"a":{"command":"a"}}}"#);
        let n = Parts::of(&nat);
        assert_eq!(n.kind, Kind::Native);
        assert_eq!(n.dirs("skills"), vec![nat.dir.join("skills")]);
        assert_eq!(n.mcp().len(), 1);
        assert!(n.hooks().0.is_empty());
    }
}
