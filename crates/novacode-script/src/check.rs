//! Codemode pre-run checks (L7.17): mistakes oxc can see before any call runs.
//! Deny by evidence only: when a check is unsure, the script passes.

use crate::codemode::{ToolInfo, declaration, js_name};
use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::{Argument, BinaryOperator, Expression, ObjectPropertyKind, PropertyKey, UnaryOperator};
use oxc_parser::Parser;
use oxc_semantic::{AstNodes, NodeId, Scoping, SemanticBuilder};
use oxc_span::{GetSpan, SourceType};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Node and browser names a model reaches for out of habit.
pub const NODE_NAMES: &[&str] = &[
    "require",
    "process",
    "fs",
    "Buffer",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "fetch",
    "window",
    "document",
    "module",
    "exports",
    "__dirname",
    "__filename",
    "navigator",
    "localStorage",
    "XMLHttpRequest",
    "Deno",
    "Bun",
];

const NO_NODE: &str = "scripts have no Node or browser APIs (no require/import, fs, process, fetch, timers); use tools.* (tools.read, tools.bash, …)";

fn node_msg(name: &str) -> String {
    format!("`{name}` doesn't exist here: {NO_NODE}")
}

const MAX_PROBLEMS: usize = 5;

/// `Object.prototype` members: every object, `tools` and `mcp.S` included, has them.
const PROTO: &[&str] = &[
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toString",
    "toLocaleString",
    "valueOf",
    "__proto__",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];

/// `id` sits in the `try` block (not the handler or finalizer) of a `try` in its
/// own function: the script expects it may throw there.
fn in_try(nodes: &AstNodes, mut id: NodeId) -> bool {
    loop {
        let up = nodes.parent_id(id);
        match nodes.kind(up) {
            AstKind::TryStatement(t) => {
                if matches!(nodes.kind(id), AstKind::BlockStatement(b) if b.span == t.block.span) {
                    return true;
                }
            }
            AstKind::Function(_) | AstKind::ArrowFunctionExpression(_) | AstKind::Program(_) => return false,
            _ => {}
        }
        if up == id {
            return false;
        }
        id = up;
    }
}

/// Problems in `js` (the wrapped script the engine runs), each naming the model's
/// line, plus the declarations of the tools they concern. `globals` = what the
/// engine's `globalThis` holds. Empty = run it.
pub fn check(js: &str, globals: &HashSet<String>, tools: &[ToolInfo]) -> (Vec<String>, Vec<String>) {
    let mut c = Checker {
        js,
        tools,
        problems: Vec::new(),
        decls: Vec::new(),
    };
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, js, SourceType::script()).parse();
    if !ret.diagnostics.is_empty() {
        // QuickJS reports syntax errors; only `import` statements get a better word.
        for (i, l) in js.lines().enumerate() {
            let t = l.trim_start();
            if (t.starts_with("import ") || t.starts_with("import{") || t.starts_with("import*"))
                && !t["import".len()..].trim_start().starts_with('(')
            {
                c.push(i + 1, node_msg("import"));
            }
        }
        return c.done();
    }
    let program = ret.program;
    let semantic = SemanticBuilder::new().with_build_nodes(true).build(&program).semantic;
    let nodes = semantic.nodes();
    let scoping = semantic.scoping();

    // a. Missing names.
    let mut skip_names = false;
    // name → (first read offset, any write, any typeof)
    let mut refs: HashMap<&str, (Option<u32>, bool, bool)> = HashMap::new();
    for node in nodes.iter() {
        match node.kind() {
            AstKind::WithStatement(_) | AstKind::ThisExpression(_) => skip_names = true,
            AstKind::ImportExpression(e) => c.push_at(e.span.start, node_msg("import()")),
            AstKind::IdentifierReference(id) => {
                let Some(rid) = id.reference_id.get() else { continue };
                let r = scoping.get_reference(rid);
                if r.symbol_id().is_some() {
                    continue;
                }
                let name = id.name.as_str();
                if matches!(name, "eval" | "Function") {
                    skip_names = true;
                }
                let mut up = nodes.parent_id(node.id());
                while let AstKind::ParenthesizedExpression(_) = nodes.kind(up) {
                    up = nodes.parent_id(up);
                }
                let parent = nodes.kind(up);
                let typeof_ = matches!(parent, AstKind::UnaryExpression(u) if u.operator == UnaryOperator::Typeof);
                let delete = matches!(parent, AstKind::UnaryExpression(u) if u.operator == UnaryOperator::Delete);
                if name == "globalThis" {
                    // Anything but a plain property read may define globals.
                    let read = matches!(
                        parent,
                        AstKind::StaticMemberExpression(_) | AstKind::ComputedMemberExpression(_)
                    ) && !matches!(
                        nodes.parent_kind(up),
                        AstKind::AssignmentExpression(_)
                            | AstKind::UpdateExpression(_)
                            | AstKind::UnaryExpression(_)
                            | AstKind::ArrayAssignmentTarget(_)
                            | AstKind::ObjectAssignmentTarget(_)
                            | AstKind::AssignmentTargetWithDefault(_)
                            | AstKind::AssignmentTargetPropertyProperty(_)
                            | AstKind::AssignmentTargetRest(_)
                            | AstKind::ForInStatement(_)
                            | AstKind::ForOfStatement(_)
                    );
                    if !read {
                        skip_names = true;
                    }
                }
                let e = refs.entry(name).or_default();
                e.1 |= r.is_write();
                e.2 |= typeof_;
                if r.is_read() && !typeof_ && !delete && e.0.is_none() && !in_try(nodes, node.id()) {
                    e.0 = Some(id.span.start);
                }
            }
            _ => {}
        }
    }
    if !skip_names {
        let mut missing: Vec<(u32, &str)> = refs
            .iter()
            .filter(|(n, (read, write, typeof_))| {
                read.is_some() && !write && !typeof_ && **n != "arguments" && !globals.contains(**n)
            })
            .map(|(n, (read, ..))| (read.unwrap_or(0), *n))
            .collect();
        missing.sort();
        for (at, n) in missing {
            let msg = if NODE_NAMES.contains(&n) {
                node_msg(n)
            } else {
                format!("`{n}` is not defined")
            };
            c.push_at(at, msg);
        }
    }

    // b. Unknown tools; c. literal argument names.
    // Feature tests: `"x" in tools` / `"x" in mcp` / `"x" in mcp.S`, or a member read
    // that isn't called (`if (tools.x)`). Keyed by ("tools" | "mcp" | server, name).
    let callees: HashSet<(u32, u32)> = nodes
        .iter()
        .filter_map(|n| match n.kind() {
            AstKind::CallExpression(call) => {
                let s = call.callee.without_parentheses().span();
                Some((s.start, s.end))
            }
            _ => None,
        })
        .collect();
    let mut tested: HashSet<(&str, &str)> = HashSet::new();
    for node in nodes.iter() {
        let (object, prop, span) = match node.kind() {
            AstKind::BinaryExpression(b) if b.operator == BinaryOperator::In => {
                let Expression::StringLiteral(s) = b.left.without_parentheses() else {
                    continue;
                };
                (&b.right, Some(s.value.as_str()), None)
            }
            AstKind::StaticMemberExpression(m) => (&m.object, Some(m.property.name.as_str()), Some(m.span)),
            AstKind::ComputedMemberExpression(m) => {
                (&m.object, m.static_property_name().map(|s| s.as_str()), Some(m.span))
            }
            _ => continue,
        };
        let Some(prop) = prop else { continue };
        match span {
            // `"x" in <object>`
            None => {
                if is_global(object, "tools", scoping) {
                    tested.insert(("tools", prop));
                } else if is_global(object, "mcp", scoping) {
                    tested.insert(("mcp", prop));
                } else if let Some(Target::Server(s)) = object
                    .without_parentheses()
                    .get_member_expr()
                    .and_then(|m| target(m.object(), m.static_property_name(), scoping))
                {
                    tested.insert((s, prop));
                }
            }
            Some(sp) if !callees.contains(&(sp.start, sp.end)) => match target(object, Some(prop), scoping) {
                Some(Target::Tool(n)) => {
                    tested.insert(("tools", n));
                }
                Some(Target::Mcp(s, n)) => {
                    tested.insert((s, n));
                }
                _ => {}
            },
            _ => {}
        }
    }
    for node in nodes.iter() {
        match node.kind() {
            // Reading an unknown server throws at run time; only `"S" in mcp` guards it.
            AstKind::StaticMemberExpression(m) => {
                if let Some(Target::Server(s)) = target(&m.object, Some(m.property.name.as_str()), scoping)
                    && !tested.contains(&("mcp", s))
                {
                    c.server(s, m.span.start);
                }
            }
            AstKind::ComputedMemberExpression(m) => {
                if let Some(Target::Server(s)) =
                    target(&m.object, m.static_property_name().map(|s| s.as_str()), scoping)
                    && !tested.contains(&("mcp", s))
                {
                    c.server(s, m.span.start);
                }
            }
            AstKind::CallExpression(call) => {
                let callee = call.callee.without_parentheses();
                let (tool, arg) = if let Expression::Identifier(id) = callee
                    && id.name == "callTool"
                    && global(id, scoping)
                {
                    match call.arguments.first() {
                        Some(Argument::StringLiteral(s)) => {
                            let name = s.value.as_str();
                            match tools.iter().find(|t| t.name == name) {
                                Some(t) => (t, call.arguments.get(1)),
                                None if tested.contains(&("tools", name)) => continue,
                                None => {
                                    c.unknown(s.span.start, &format!("callTool(\"{name}\")"), name, tools.iter());
                                    continue;
                                }
                            }
                        }
                        _ => continue,
                    }
                } else {
                    let at = callee.span().start;
                    match callee
                        .get_member_expr()
                        .and_then(|m| target(m.object(), m.static_property_name(), scoping))
                    {
                        Some(Target::Tool(n)) => match tools.iter().find(|t| t.name == n) {
                            Some(t) => (t, call.arguments.first()),
                            None => {
                                if !tested.contains(&("tools", n)) {
                                    c.unknown(at, &format!("tools.{n}"), n, tools.iter());
                                }
                                continue;
                            }
                        },
                        Some(Target::Mcp(s, n)) => match find_mcp(tools, s, n) {
                            Some(t) => (t, call.arguments.first()),
                            None => {
                                // An unknown server is reported on its own.
                                if tools.iter().any(|t| t.namespace.as_deref() == Some(s)) && !tested.contains(&(s, n))
                                {
                                    c.unknown(at, &format!("mcp.{s}.{n}"), n, tools.iter());
                                }
                                continue;
                            }
                        },
                        _ => continue,
                    }
                };
                if let Some(Argument::ObjectExpression(o)) = arg {
                    let mut keys = Vec::new();
                    for p in &o.properties {
                        let ObjectPropertyKind::ObjectProperty(p) = p else {
                            keys.clear();
                            break;
                        };
                        match &p.key {
                            PropertyKey::StaticIdentifier(k) if !p.computed => keys.push(k.name.as_str()),
                            PropertyKey::StringLiteral(k) => keys.push(k.value.as_str()),
                            _ => {
                                keys.clear();
                                break;
                            }
                        }
                    }
                    let complete = keys.len() == o.properties.len();
                    if complete {
                        c.args(call.span.start, tool, &keys);
                    }
                }
            }
            _ => {}
        }
    }
    c.done()
}

struct Checker<'a> {
    js: &'a str,
    tools: &'a [ToolInfo],
    problems: Vec<String>,
    decls: Vec<String>,
}

enum Target<'a> {
    Tool(&'a str),
    Server(&'a str),
    Mcp(&'a str, &'a str),
}

fn global(id: &oxc_ast::ast::IdentifierReference, scoping: &Scoping) -> bool {
    id.reference_id
        .get()
        .is_some_and(|r| scoping.get_reference(r).symbol_id().is_none())
}

fn is_global(e: &Expression, name: &str, scoping: &Scoping) -> bool {
    matches!(e.without_parentheses(), Expression::Identifier(id) if id.name == name && global(id, scoping))
}

/// What `object.prop` names: `tools.X`, `mcp.S` or `mcp.S.T`.
fn target<'a>(object: &Expression<'a>, prop: Option<&'a str>, scoping: &Scoping) -> Option<Target<'a>> {
    let prop = prop?;
    if prop == "then" || PROTO.contains(&prop) {
        return None;
    }
    if is_global(object, "tools", scoping) {
        return (prop != "call").then_some(Target::Tool(prop));
    }
    if is_global(object, "mcp", scoping) {
        return Some(Target::Server(prop));
    }
    let inner = object.without_parentheses().get_member_expr()?;
    if is_global(inner.object(), "mcp", scoping) {
        return Some(Target::Mcp(inner.static_property_name()?, prop));
    }
    None
}

fn find_mcp<'t>(tools: &'t [ToolInfo], server: &str, short: &str) -> Option<&'t ToolInfo> {
    tools.iter().find(|t| {
        t.namespace.as_deref() == Some(server)
            && t.name.len() > server.len() + 2
            && &t.name[server.len() + 2..] == short
    })
}

impl Checker<'_> {
    fn push(&mut self, line: usize, msg: String) {
        // The wrapper adds one line.
        let p = format!("line {}: {msg}", line.saturating_sub(1).max(1));
        if !self.problems.contains(&p) {
            self.problems.push(p);
        }
    }

    fn push_at(&mut self, offset: u32, msg: String) {
        let line = self.js[..(offset as usize).min(self.js.len())].matches('\n').count() + 1;
        self.push(line, msg);
    }

    fn decl(&mut self, t: &ToolInfo) {
        let d = declaration(t);
        if !self.decls.contains(&d) {
            self.decls.push(d);
        }
    }

    fn server(&mut self, s: &str, at: u32) {
        let tools = self.tools;
        if tools.iter().any(|t| t.namespace.as_deref() == Some(s)) {
            return;
        }
        let mut servers: Vec<(String, String)> = tools
            .iter()
            .filter_map(|t| t.namespace.clone())
            .map(|s| (format!("mcp.{s}"), s))
            .collect();
        servers.sort();
        servers.dedup();
        let near = nearest(s, servers.iter().map(|(js, s)| (js.as_str(), s.as_str())));
        self.push_at(at, unknown_msg(&format!("mcp.{s}"), "MCP server", &near));
    }

    fn unknown<'t>(&mut self, at: u32, what: &str, name: &str, tools: impl Iterator<Item = &'t ToolInfo>) {
        let named: Vec<(String, &str)> = tools.map(|t| (js_name(t), short(t))).collect();
        let near = nearest(name, named.iter().map(|(js, s)| (js.as_str(), *s)));
        self.push_at(at, unknown_msg(what, "tool", &near));
    }

    fn args(&mut self, at: u32, t: &ToolInfo, keys: &[&str]) {
        let props = t.schema["properties"].as_object();
        let is_prop = |k: &str| props.is_some_and(|p| p.contains_key(k));
        let required: Vec<&str> = t.schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        let mut used = HashSet::new();
        let js = js_name(t);
        for r in required {
            if keys.contains(&r) {
                continue;
            }
            let guess = keys
                .iter()
                .find(|k| !is_prop(k) && !used.contains(*k) && likely(k, r))
                .copied();
            match guess {
                Some(k) => {
                    used.insert(k);
                    self.push_at(at, format!("{js} has no `{k}` (did you mean `{r}`?)"));
                }
                None if t.namespace.is_some() => {
                    self.push_at(at, format!("{js} is missing required `{r}`"));
                }
                None => continue,
            }
            self.decl(t);
        }
    }

    fn done(self) -> (Vec<String>, Vec<String>) {
        let mut p = self.problems;
        p.truncate(MAX_PROBLEMS);
        (p, self.decls)
    }
}

fn short(t: &ToolInfo) -> &str {
    match &t.namespace {
        Some(ns) => &t.name[ns.len() + 2..],
        None => &t.name,
    }
}

fn unknown_msg(what: &str, kind: &str, near: &[String]) -> String {
    if near.is_empty() {
        format!("`{what}` is not a {kind}; find tools with searchTools(\"…\")")
    } else {
        format!("`{what}` is not a {kind} (nearest: {})", near.join(", "))
    }
}

/// Up to 3 JS names whose short name is close to `want`, nearest first.
fn nearest<'a>(want: &str, names: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let w = want.to_lowercase();
    let mut hits: Vec<(usize, &str)> = names
        .filter_map(|(js, s)| {
            let s = s.to_lowercase();
            let d = lev(&w, &s);
            let sub = !w.is_empty() && !s.is_empty() && (s.contains(&w) || w.contains(&s));
            (sub || d <= 2.max(w.len() / 3)).then_some((if sub { d.min(1) } else { d }, js))
        })
        .collect();
    hits.sort();
    hits.dedup_by(|a, b| a.1 == b.1);
    hits.into_iter().take(3).map(|(_, js)| js.to_string()).collect()
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// `key` looks like a misspelling of `want`.
pub fn likely(key: &str, want: &str) -> bool {
    let (k, w) = (norm(key), norm(want));
    k == w
        || (!k.is_empty() && !w.is_empty() && (k.contains(&w) || w.contains(&k)))
        || lev(&key.to_lowercase(), &want.to_lowercase()) <= 2
}

/// Edit distance.
pub fn lev(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + (ca != *cb) as usize).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// MCP arguments against the tool's input schema (L7.17): required, types, enums,
/// nested properties and items. Parts using `$ref`, `oneOf`, `anyOf`, `allOf`,
/// `not` or `if` are skipped. Keys that aren't properties are only named on error.
pub fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    let mut errs = Vec::new();
    let mut extras = Vec::new();
    walk(schema, args, "", &mut errs, &mut extras);
    if errs.is_empty() {
        return Ok(());
    }
    errs.extend(extras);
    Err(errs.join("; "))
}

fn walk(s: &Value, v: &Value, path: &str, errs: &mut Vec<String>, extras: &mut Vec<String>) {
    let Some(o) = s.as_object() else { return };
    if ["$ref", "oneOf", "anyOf", "allOf", "not", "if"]
        .iter()
        .any(|k| o.contains_key(*k))
    {
        return;
    }
    let at = |p: &str| {
        if p.is_empty() {
            "the arguments".to_string()
        } else {
            format!("\"{p}\"")
        }
    };
    let types: Vec<&str> = match &s["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(a) => a.iter().filter_map(|t| t.as_str()).collect(),
        _ => vec![],
    };
    // OpenAPI-style `nullable: true` allows null whatever the type.
    if v.is_null() && s["nullable"] == Value::Bool(true) {
        return;
    }
    // Lax servers coerce numeric strings (`"5"`), so those pass unchanged.
    let num = v.as_str().and_then(|x| x.parse::<f64>().ok()).filter(|f| f.is_finite());
    let fits = |t: &str| match t {
        "string" => v.is_string(),
        "number" => v.is_number() || num.is_some(),
        "integer" => {
            v.is_i64()
                || v.is_u64()
                || v.as_f64().is_some_and(|f| f.fract() == 0.0)
                || num.is_some_and(|f| f.fract() == 0.0)
        }
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    };
    if !types.is_empty() && !types.iter().any(|t| fits(t)) {
        errs.push(format!("{} must be {}", at(path), types.join(" or ")));
        return;
    }
    if let Some(e) = s["enum"].as_array()
        && !e.contains(v)
    {
        let all: Vec<String> = e.iter().map(Value::to_string).collect();
        errs.push(format!("{} must be one of {}", at(path), all.join(", ")));
        return;
    }
    let join = |k: &str| {
        if path.is_empty() {
            k.to_string()
        } else {
            format!("{path}.{k}")
        }
    };
    match v {
        Value::Object(m) => {
            let props = s["properties"].as_object();
            let missing: Vec<&str> = s["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str())
                .filter(|r| !m.contains_key(*r))
                .collect();
            for r in &missing {
                errs.push(format!("missing required \"{}\"", join(r)));
            }
            let open = !matches!(s.get("additionalProperties"), None | Some(Value::Bool(false)));
            for (k, val) in m {
                match props.and_then(|p| p.get(k)) {
                    Some(ps) => walk(ps, val, &join(k), errs, extras),
                    None if props.is_some() && !open => {
                        let guess = missing
                            .iter()
                            .copied()
                            .chain(props.into_iter().flat_map(|p| p.keys().map(String::as_str)))
                            .find(|w| likely(k, w));
                        extras.push(match guess {
                            Some(w) => format!("\"{}\" is not a parameter (did you mean \"{}\"?)", join(k), join(w)),
                            None => format!("\"{}\" is not a parameter", join(k)),
                        });
                    }
                    None => {}
                }
            }
        }
        Value::Array(a) => {
            for (i, item) in a.iter().enumerate() {
                walk(&s["items"], item, &format!("{path}[{i}]"), errs, extras);
            }
        }
        _ => {}
    }
}

/// One line of advice for a failed script's error, for known error classes.
pub fn hint(err: &str, code: &str) -> Option<&'static str> {
    // rquickjs prints every error class as `Error: `.
    let first = err.lines().next().unwrap_or("");
    let msg = first.strip_prefix("Error: ").unwrap_or(first);
    let syntax = || {
        let alloc = Allocator::default();
        !Parser::new(&alloc, code, SourceType::script())
            .parse()
            .diagnostics
            .is_empty()
    };
    if NODE_NAMES
        .iter()
        .any(|n| msg.starts_with(&format!("{n} is not defined")) || msg.starts_with(&format!("'{n}' is not defined")))
    {
        Some(NO_NODE)
    } else if msg.starts_with("bash: ") {
        Some("tools.bash throws on a non-zero exit; catch it, or add `|| true` when a non-zero exit is expected")
    } else if err.contains("at parse (native)") {
        Some("tool output is plain text; JSON.parse only output you know is JSON")
    } else if code.contains('`') && syntax() {
        Some("a backtick inside a template string ends it; build such text with normal strings")
    } else if first.contains("not a function") || first.contains("cannot read property") {
        Some(
            "tools.* return text (split or JSON.parse it); MCP tools with an output schema return objects, results with images are {text, images}: check typeof",
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_and_likely() {
        assert_eq!(lev("kitten", "sitting"), 3);
        assert!(likely("file_path", "path") && likely("Path", "path") && likely("qury", "query"));
        assert!(!likely("command", "path"));
    }
}
