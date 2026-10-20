//! Skills (L4.10) and prompt templates (L4.11): markdown files with frontmatter.
//!
//! Skills follow the Agent Skills spec: the prompt lists each skill's name,
//! description and file, and the model reads the file when a task matches
//! (progressive disclosure). `/skill:name args` inlines one by hand. Templates
//! are `/name args` shortcuts with bash-style argument placeholders.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// One loader rule for skills, templates and agents (L7.12): the same real file
/// (a symlink, two folders pointing at one place) counts once, silently; a
/// different file with a name already taken loses to the earlier folder, and is
/// listed in `shadowed` for `novacode doctor` instead of a warning on every start.
#[derive(Default)]
pub struct Seen {
    files: HashSet<PathBuf>,
    names: BTreeMap<String, PathBuf>,
    pub shadowed: Vec<String>,
}

impl Seen {
    /// The name `name` at `path` loads under, or `None`. A package's file whose
    /// name is taken loads as `<package>:<name>` (L7.14).
    pub fn admit(&mut self, kind: &str, name: &str, path: &Path) -> Option<String> {
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !self.files.insert(real.clone()) {
            return None;
        }
        if let Some(first) = self.names.get(name) {
            let first = first.display().to_string();
            let ns = crate::plugin::owner_name(path)
                .map(|p| format!("{p}:{name}"))
                .filter(|n| !self.names.contains_key(n));
            self.shadowed.push(format!(
                "{kind} `{name}`: {} is shadowed by {first}{}",
                real.display(),
                ns.as_ref().map(|n| format!("; it loads as `{n}`")).unwrap_or_default()
            ));
            let ns = ns?;
            self.names.insert(ns.clone(), real);
            return Some(ns);
        }
        self.names.insert(name.to_string(), real);
        Some(name.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// `disable-model-invocation`: only `/skill:name` uses it; not listed to the model.
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Top-level `key: value` pairs and the body. Handles quoted values and `|`/`>`
/// block scalars; nested maps are skipped. No frontmatter = empty map.
pub fn frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut map = BTreeMap::new();
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (map, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (map, text);
    };
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
    let mut lines = rest[..end].lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let value = if v.starts_with(['|', '>']) {
            let mut parts = Vec::new();
            while let Some(next) = lines.next_if(|l| l.starts_with([' ', '\t']) || l.trim().is_empty()) {
                parts.push(next.trim());
            }
            let sep = if v.starts_with('|') { "\n" } else { " " };
            parts.join(sep).trim().to_string()
        } else {
            v.trim_matches(|c| c == '"' || c == '\'').to_string()
        };
        map.insert(k.trim().to_string(), value);
    }
    (map, body)
}

/// Skills from `dirs`, earlier folders winning a name clash ([`Seen`]). Sorted by
/// name, so the prompt renders the same every time. Returns problems to show
/// (a skill without a description) and the shadowed ones (for `doctor`).
pub fn skills(dirs: &[PathBuf]) -> (Vec<Skill>, Vec<String>, Vec<String>) {
    let mut found: BTreeMap<String, Skill> = BTreeMap::new();
    let mut warn = Vec::new();
    let mut seen = Seen::default();
    for dir in dirs {
        let mut files = Vec::new();
        scan(dir, true, &mut files);
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (fm, _) = frontmatter(&text);
            let declared = path.file_name().is_some_and(|n| n == "SKILL.md");
            let Some(description) = fm.get("description").filter(|d| !d.trim().is_empty()) else {
                if declared {
                    warn.push(format!("skill {} has no description; skipped", path.display()));
                }
                continue;
            };
            let name = fm.get("name").cloned().unwrap_or_else(|| {
                let dir = if declared { path.parent() } else { Some(path.as_path()) };
                dir.and_then(|d| d.file_stem())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            let Some(name) = seen.admit("skill", &name, &path) else {
                continue;
            };
            found.insert(
                name.clone(),
                Skill {
                    name,
                    description: description.chars().take(1024).collect(),
                    path,
                    hidden: fm.get("disable-model-invocation").is_some_and(|v| v == "true"),
                },
            );
        }
    }
    (found.into_values().collect(), warn, seen.shadowed)
}

/// pi's discovery: a folder with `SKILL.md` is one skill (no deeper); otherwise
/// the root's own `.md` files count, and subfolders are searched for `SKILL.md`.
fn scan(dir: &Path, root: bool, out: &mut Vec<PathBuf>) {
    let skill = dir.join("SKILL.md");
    if skill.is_file() {
        out.push(skill);
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        if p.is_dir() {
            scan(&p, false, out);
        } else if root && name.ends_with(".md") {
            out.push(p);
        }
    }
}

/// Longest skill description in the prompt; the full text stays in its file.
const DESCRIPTION_CAP: usize = 200;

/// The `skills` prompt section body; empty when no skill is listed. Compact: one
/// line per skill, grouped by folder so paths aren't repeated, descriptions capped
/// (it is resent with every model call).
pub fn render_skills(skills: &[Skill]) -> String {
    let listed: Vec<&Skill> = skills.iter().filter(|s| !s.hidden).collect();
    if listed.is_empty() {
        return String::new();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let show = |p: &Path| match home.as_deref().and_then(|h| p.strip_prefix(h).ok()) {
        Some(r) => format!("~/{}", r.display()),
        None => p.display().to_string(),
    };
    let mut s = String::from(
        "Skills: specialized instructions for specific tasks. When a task matches one, read its file first: `<folder>/<name>/SKILL.md`, or the path in brackets. Resolve relative paths in it against the skill's own folder.",
    );
    let mut folder: Option<&Path> = None;
    for k in listed {
        // `<folder>/<name>/SKILL.md` is implied; anything else shows its path.
        let implied = k.path.file_name().is_some_and(|f| f == "SKILL.md")
            && k.path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|d| d.to_string_lossy() == k.name);
        let group = if implied {
            k.path.parent().and_then(Path::parent)
        } else {
            k.path.parent()
        };
        if group != folder {
            folder = group;
            s.push_str(&format!("\n{}/", group.map(show).unwrap_or_default()));
        }
        let at = if implied {
            String::new()
        } else {
            format!(
                " [{}]",
                k.path
                    .strip_prefix(group.unwrap_or(Path::new("")))
                    .unwrap_or(&k.path)
                    .display()
            )
        };
        s.push_str(&format!("\n- {}{at}: {}", k.name, cap(&k.description)));
    }
    // Claude Code plugin skills name their package folder with this variable (L7.14).
    if skills.iter().any(|k| {
        !k.hidden && crate::plugin::Parts::owning(&k.path).is_some_and(|p| p.kind == crate::plugin::Kind::Claude)
    }) {
        s.push_str(
            "\nIn a skill from a package (`…/packages/<name>/…`), `${CLAUDE_PLUGIN_ROOT}` means that package's folder.",
        );
    }
    s
}

/// Whitespace collapsed, cut at a word boundary under [`DESCRIPTION_CAP`].
fn cap(d: &str) -> String {
    let d = d.split_whitespace().collect::<Vec<_>>().join(" ");
    if d.chars().count() <= DESCRIPTION_CAP {
        return d;
    }
    let cut: String = d.chars().take(DESCRIPTION_CAP).collect();
    format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(a, _)| a))
}

/// Templates from `dirs` (top-level `.md` files, or a `.md` file itself), earlier
/// folders winning ([`Seen`]). Returns the shadowed ones too (for `doctor`).
pub fn templates(dirs: &[PathBuf]) -> (Vec<Template>, Vec<String>) {
    let mut found: BTreeMap<String, Template> = BTreeMap::new();
    let mut seen = Seen::default();
    for dir in dirs {
        let mut files: Vec<PathBuf> = if dir.is_file() {
            vec![dir.clone()]
        } else {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            rd.flatten().map(|e| e.path()).collect()
        };
        files.sort();
        for p in files {
            if p.extension().is_none_or(|e| e != "md") || !p.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let name = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let Some(name) = seen.admit("prompt", &name, &p) else {
                continue;
            };
            let text = crate::plugin::fill_for(&p, text);
            let (fm, body) = frontmatter(&text);
            let description = fm.get("description").cloned().unwrap_or_else(|| {
                let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
                let short: String = first.chars().take(60).collect();
                if first.chars().count() > 60 {
                    format!("{short}...")
                } else {
                    short
                }
            });
            found.insert(
                name.clone(),
                Template {
                    name,
                    description,
                    body: body.to_string(),
                },
            );
        }
    }
    (found.into_values().collect(), seen.shadowed)
}

/// Expand `/skill:name args` and `/template args`; anything else is returned as is.
pub fn expand(text: &str, skills: &[Skill], templates: &[Template]) -> String {
    let Some(rest) = text.strip_prefix('/') else {
        return text.to_string();
    };
    let (cmd, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    if let Some(name) = cmd.strip_prefix("skill:") {
        let Some(k) = skills.iter().find(|k| k.name == name) else {
            return text.to_string();
        };
        let Ok(raw) = std::fs::read_to_string(&k.path) else {
            return text.to_string();
        };
        let raw = crate::plugin::fill_for(&k.path, raw);
        let base = k.path.parent().unwrap_or(Path::new("/"));
        let block = format!(
            "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
            k.name,
            k.path.display(),
            base.display(),
            frontmatter(&raw).1.trim()
        );
        return if args.is_empty() {
            block
        } else {
            format!("{block}\n\n{args}")
        };
    }
    match templates.iter().find(|t| t.name == cmd) {
        Some(t) => substitute(&t.body, &split_args(args)),
        None => text.to_string(),
    }
}

/// Shell-like words: quotes group, `""` is an empty argument.
fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut started, mut quote) = (Vec::new(), String::new(), false, None);
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// `$1`, `$@`/`$ARGUMENTS`, `${N:-default}`, `${@:-default}`, `${@:N}`, `${@:N:L}` (pi's set).
/// Values are not substituted again.
fn substitute(body: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let nth = |n: usize| args.get(n.wrapping_sub(1)).cloned().unwrap_or_default();
    let mut out = String::new();
    let mut rest = body;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(inner) = rest.strip_prefix('{').and_then(|r| r.split_once('}')) {
            let (expr, after) = inner;
            let value = if let Some((target, default)) = expr.split_once(":-") {
                let v = match target {
                    "@" | "ARGUMENTS" => all.clone(),
                    n => n.parse().map(nth).unwrap_or_default(),
                };
                Some(if v.is_empty() { default.to_string() } else { v })
            } else if let Some(slice) = expr.strip_prefix("@:") {
                let mut it = slice.split(':').map(|x| x.parse::<usize>().ok());
                match (it.next().flatten(), it.next()) {
                    (Some(start), len) => {
                        let start = start.max(1) - 1;
                        let take = len.flatten().unwrap_or(usize::MAX);
                        Some(
                            args.iter()
                                .skip(start)
                                .take(take)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(" "),
                        )
                    }
                    _ => None,
                }
            } else {
                None
            };
            match value {
                Some(v) => {
                    out.push_str(&v);
                    rest = after;
                }
                None => out.push('$'),
            }
            continue;
        }
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '@')
            .collect();
        let (value, used) = if rest.starts_with('@') {
            (Some(all.clone()), 1)
        } else if word.starts_with("ARGUMENTS") {
            (Some(all.clone()), "ARGUMENTS".len())
        } else {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            match digits.parse::<usize>() {
                Ok(n) => (Some(nth(n)), digits.len()),
                Err(_) => (None, 0),
            }
        };
        match value {
            Some(v) => {
                out.push_str(&v);
                rest = &rest[used..];
            }
            None => out.push('$'),
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_handles_blocks_and_quotes() {
        let (fm, body) = frontmatter(
            "---\nname: humanizer\ndescription: |\n  Rewrite text.\n  Keep meaning.\nlicense: \"MIT\"\nmetadata:\n  version: \"3\"\nfolded: >\n  a\n  b\n---\n\nBody here\n",
        );
        assert_eq!(fm["description"], "Rewrite text.\nKeep meaning.");
        assert_eq!(fm["license"], "MIT");
        assert_eq!(fm["folded"], "a b");
        assert!(!fm.contains_key("version"), "nested keys are skipped");
        assert_eq!(body, "Body here\n");
        assert_eq!(frontmatter("no frontmatter").1, "no frontmatter");
    }

    #[test]
    fn skills_discovery_order_and_render() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        let w = |p: PathBuf, t: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        w(a.join("zeta/SKILL.md"), "---\ndescription: last <b>\n---\nZ body");
        w(
            a.join("alpha/SKILL.md"),
            "---\nname: alpha\ndescription: first\n---\nA body",
        );
        w(
            a.join("alpha/nested/SKILL.md"),
            "---\ndescription: not found: SKILL.md stops the search\n---\n",
        );
        w(a.join("loose.md"), "---\ndescription: a root .md file\n---\n");
        w(
            a.join("secret/SKILL.md"),
            "---\ndescription: hidden\ndisable-model-invocation: true\n---\nS",
        );
        w(b.join("alpha/SKILL.md"), "---\ndescription: shadowed\n---\n");
        w(b.join("nodesc/SKILL.md"), "---\nname: x\n---\n");
        // The same folder twice (a symlink): loaded once, nothing reported.
        std::os::unix::fs::symlink(&a, d.path().join("link")).unwrap();
        let (s, warn, shadowed) = skills(&[a.clone(), d.path().join("link"), b]);
        let names: Vec<&str> = s.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["alpha", "loose", "secret", "zeta"]);
        assert_eq!(s[0].description, "first");
        assert_eq!(warn.len(), 1, "only the missing description: {warn:?}");
        assert_eq!(shadowed.len(), 1, "the other alpha, for doctor: {shadowed:?}");
        let r = render_skills(&s);
        assert!(
            r.contains("\n- zeta: last <b>") && r.contains("\n- loose [loose.md]: a root .md file"),
            "{r}"
        );
        assert_eq!(
            r.matches(&format!("\n{}/", a.display())).count(),
            1,
            "one folder line: {r}"
        );
        assert!(
            cap(&"word ".repeat(100)).ends_with("word…")
                && cap(&"word ".repeat(100)).chars().count() <= DESCRIPTION_CAP + 1
        );
        assert!(!r.contains("secret"), "hidden skills aren't listed");
        let x = expand("/skill:secret do it", &s, &[]);
        assert!(
            x.starts_with("<skill name=\"secret\"") && x.contains("\nS\n</skill>\n\ndo it"),
            "{x}"
        );
        assert_eq!(expand("/skill:nope", &s, &[]), "/skill:nope");
    }

    #[test]
    fn template_arguments() {
        let t = |body: &str| {
            vec![Template {
                name: "t".into(),
                description: String::new(),
                body: body.into(),
            }]
        };
        let e = |body: &str, args: &str| expand(&format!("/t {args}"), &[], &t(body));
        assert_eq!(e("fix $1 in $2", "bug 'src/a b.rs'"), "fix bug in src/a b.rs");
        assert_eq!(e("all: $@ / $ARGUMENTS", "x y"), "all: x y / x y");
        assert_eq!(e("${1:-none} ${3:-def}", "a"), "a def");
        assert_eq!(e("${@:2} | ${@:1:2}", "a b c d"), "b c d | a b");
        assert_eq!(e("cost $5 is $X", ""), "cost  is $X");
        assert_eq!(e("${@:-nothing}", ""), "nothing");
        assert_eq!(expand("/other x", &[], &t("y")), "/other x");
        assert_eq!(expand("plain", &[], &t("y")), "plain");
    }
}
