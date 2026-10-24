//! Command risk gate (L6.2): a deterministic blast-radius classifier for shell
//! commands, after jcode-command-risk. Biased toward recall: what it can't
//! parse is escalated, never waved through. Defense in depth, not a sandbox.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Read-only; allowed in read-only mode.
    Safe,
    /// Changes something ordinary (writes a file, runs a program).
    Low,
    /// Destroys data or history in bulk; needs the user (or a justification headless).
    Confirm,
    /// Rewrites or deletes shared state off this machine (a remote's history), which
    /// no local checkpoint can undo: asks even in auto mode.
    Shared,
    /// Targets the system, the home folder or credentials; never runs.
    Catastrophic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Risk {
    pub level: Level,
    /// Why, for the user and the model. Empty for Safe/Low.
    pub reason: String,
}

impl Risk {
    fn new(level: Level, reason: impl Into<String>) -> Self {
        Self {
            level,
            reason: reason.into(),
        }
    }
    fn low() -> Self {
        Self::new(Level::Low, "")
    }
    fn safe() -> Self {
        Self::new(Level::Safe, "")
    }
    fn max(self, other: Self) -> Self {
        if other.level > self.level { other } else { self }
    }
}

/// What the classifier needs to resolve paths.
pub struct Ctx<'a> {
    pub cwd: &'a Path,
    pub home: &'a Path,
    /// Extra protected paths from settings (absolute or `~/…`).
    pub protected: &'a [String],
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    /// `;` `&&` `||` `|` `&` or newline.
    Sep(String),
    /// `>` or `>>` (any fd).
    Redirect,
}

/// Split into words and operators, honouring quotes. `$(…)` and backtick
/// bodies are returned separately so they can be classified too.
/// `$(…)` and backtick bodies in an unquoted heredoc body.
fn expansions(body: &str) -> Option<Vec<String>> {
    let c: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '$' if c.get(i + 1) == Some(&'(') => {
                let (b, end) = paren_body(&c, i + 2)?;
                out.push(b);
                i = end;
            }
            '`' => {
                let end = c[i + 1..].iter().position(|&x| x == '`')? + i + 1;
                out.push(c[i + 1..end].iter().collect());
                i = end;
            }
            _ => {}
        }
        i += 1;
    }
    Some(out)
}

fn lex(src: &str) -> Option<(Vec<Tok>, Vec<String>)> {
    let mut toks = Vec::new();
    let mut subs = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    // Heredocs opened on this line: (delimiter, strip leading tabs, delimiter quoted).
    let mut heredocs: Vec<(String, bool, bool)> = Vec::new();
    let c: Vec<char> = src.chars().collect();
    let mut i = 0;
    let flush = |word: &mut String, in_word: &mut bool, toks: &mut Vec<Tok>| {
        if *in_word {
            toks.push(Tok::Word(std::mem::take(word)));
            *in_word = false;
        }
    };
    while i < c.len() {
        let ch = c[i];
        match ch {
            '\'' => {
                let end = c[i + 1..].iter().position(|&x| x == '\'')? + i + 1;
                word.extend(&c[i + 1..end]);
                in_word = true;
                i = end;
            }
            '"' => {
                i += 1;
                loop {
                    match *c.get(i)? {
                        '"' => break,
                        '\\' => {
                            i += 1;
                            word.push(*c.get(i)?);
                        }
                        '$' if c.get(i + 1) == Some(&'(') => {
                            let (body, end) = paren_body(&c, i + 2)?;
                            subs.push(body.clone());
                            word.push_str(&format!("$({body})"));
                            i = end;
                        }
                        '`' => {
                            let end = c[i + 1..].iter().position(|&x| x == '`')? + i + 1;
                            subs.push(c[i + 1..end].iter().collect());
                            i = end;
                        }
                        x => word.push(x),
                    }
                    i += 1;
                }
                in_word = true;
            }
            '\\' => {
                i += 1;
                if let Some(&n) = c.get(i)
                    && n != '\n'
                {
                    word.push(n);
                    in_word = true;
                }
            }
            '$' if c.get(i + 1) == Some(&'(') => {
                let (body, end) = paren_body(&c, i + 2)?;
                subs.push(body.clone());
                word.push_str(&format!("$({body})"));
                in_word = true;
                i = end;
            }
            '`' => {
                let end = c[i + 1..].iter().position(|&x| x == '`')? + i + 1;
                subs.push(c[i + 1..end].iter().collect());
                in_word = true;
                i = end;
            }
            '(' | ')' | '{' | '}' if !in_word => {
                // Subshells and groups: treat as separators so their commands are classified.
                toks.push(Tok::Sep(ch.to_string()));
            }
            ' ' | '\t' => flush(&mut word, &mut in_word, &mut toks),
            '#' if !in_word => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            ';' | '\n' => {
                flush(&mut word, &mut in_word, &mut toks);
                toks.push(Tok::Sep(";".into()));
                if ch == '\n' && !heredocs.is_empty() {
                    // Heredoc bodies are data, not commands. Unquoted ones still expand
                    // `$(…)` and backticks, so those are classified.
                    i += 1;
                    for (delim, strip, quoted) in std::mem::take(&mut heredocs) {
                        let mut body = String::new();
                        while i < c.len() {
                            let end = c[i..].iter().position(|&x| x == '\n').map_or(c.len(), |p| p + i);
                            let line: String = c[i..end].iter().collect();
                            i = (end + 1).min(c.len());
                            let l = if strip { line.trim_start_matches('\t') } else { &line };
                            if l == delim {
                                break;
                            }
                            body.push_str(&line);
                            body.push('\n');
                        }
                        if !quoted {
                            subs.extend(expansions(&body)?);
                        }
                    }
                    continue;
                }
            }
            '&' | '|' => {
                // `&>` / `>&` redirect forms.
                if ch == '&' && c.get(i + 1) == Some(&'>') {
                    flush(&mut word, &mut in_word, &mut toks);
                    toks.push(Tok::Redirect);
                    i += 2;
                    if c.get(i) == Some(&'>') {
                        i += 1;
                    }
                    continue;
                }
                flush(&mut word, &mut in_word, &mut toks);
                let double = c.get(i + 1) == Some(&ch);
                toks.push(Tok::Sep(if double { format!("{ch}{ch}") } else { ch.to_string() }));
                if double {
                    i += 1;
                }
            }
            '>' => {
                // A word of only digits right before `>` is an fd number (2>).
                if in_word && word.chars().all(|d| d.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                flush(&mut word, &mut in_word, &mut toks);
                if c.get(i + 1) == Some(&'>') {
                    i += 1;
                }
                if c.get(i + 1) == Some(&'&') {
                    // `>&2`: duplicate an fd, not a file.
                    i += 2;
                    while c.get(i).is_some_and(|d| d.is_ascii_digit() || *d == '-') {
                        i += 1;
                    }
                    continue;
                }
                if c.get(i + 1) == Some(&'|') {
                    i += 1;
                }
                toks.push(Tok::Redirect);
            }
            '<' if c.get(i + 1) == Some(&'<') && c.get(i + 2) != Some(&'<') => {
                // `<<DELIM` / `<<-DELIM`: the body starts on the next line.
                flush(&mut word, &mut in_word, &mut toks);
                i += 2;
                let strip = c.get(i) == Some(&'-');
                if strip {
                    i += 1;
                }
                while matches!(c.get(i), Some(' ' | '\t')) {
                    i += 1;
                }
                let mut delim = String::new();
                let mut quoted = false;
                while let Some(&d) = c.get(i) {
                    match d {
                        '\'' | '"' => {
                            quoted = true;
                            let end = c[i + 1..].iter().position(|&x| x == d)? + i + 1;
                            delim.extend(&c[i + 1..end]);
                            i = end + 1;
                        }
                        '\\' => {
                            quoted = true;
                            delim.extend(c.get(i + 1));
                            i += 2;
                        }
                        ' ' | '\t' | '\n' | ';' | '|' | '&' | '<' | '>' | '(' | ')' => break,
                        x => {
                            delim.push(x);
                            i += 1;
                        }
                    }
                }
                if delim.is_empty() {
                    return None;
                }
                heredocs.push((delim, strip, quoted));
                continue;
            }
            '<' => flush(&mut word, &mut in_word, &mut toks),
            x => {
                word.push(x);
                in_word = true;
            }
        }
        i += 1;
    }
    flush(&mut word, &mut in_word, &mut toks);
    Some((toks, subs))
}

/// Body of `$( … )` starting at `start` (just after the paren); returns (body, index of `)`).
fn paren_body(c: &[char], start: usize) -> Option<(String, usize)> {
    let mut depth = 1;
    let mut i = start;
    while i < c.len() {
        match c[i] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((c[start..i].iter().collect(), i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The simple commands of a command line as text, for rule matching (L6.1).
/// `None` when the line can't be parsed.
pub fn segments(src: &str) -> Option<Vec<String>> {
    let (toks, _) = lex(src)?;
    let mut out = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for t in toks {
        match t {
            Tok::Word(w) => cur.push(w),
            Tok::Sep(_) => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur).join(" "));
                }
            }
            Tok::Redirect => cur.push(">".into()),
        }
    }
    if !cur.is_empty() {
        out.push(cur.join(" "));
    }
    Some(out)
}

pub fn classify(src: &str, ctx: &Ctx) -> Risk {
    classify_depth(src, ctx, 0)
}

fn classify_depth(src: &str, ctx: &Ctx, depth: u32) -> Risk {
    if depth > 4 {
        return Risk::new(Level::Confirm, "commands nested too deeply to check");
    }
    let compact: String = src.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains(":(){") || compact.contains(":|:&") {
        return Risk::new(Level::Catastrophic, "fork bomb");
    }
    let Some((toks, subs)) = lex(src) else {
        return Risk::new(Level::Confirm, "the command could not be parsed (unbalanced quotes?)");
    };
    let mut risk = Risk::safe();
    for s in &subs {
        risk = risk.max(classify_depth(s, ctx, depth + 1));
    }
    let mut words: Vec<String> = Vec::new();
    let mut piped = false;
    let mut iter = toks.into_iter().peekable();
    loop {
        match iter.next() {
            Some(Tok::Word(w)) => words.push(w),
            Some(Tok::Redirect) => {
                if let Some(Tok::Word(target)) = iter.next() {
                    risk = risk.max(redirect(&target, ctx));
                } else {
                    risk = risk.max(Risk::new(Level::Confirm, "redirect without a target"));
                }
            }
            sep @ (Some(Tok::Sep(_)) | None) => {
                if !words.is_empty() {
                    risk = risk.max(command(&words, piped, ctx, depth));
                }
                words.clear();
                match sep {
                    Some(Tok::Sep(s)) => piped = s == "|",
                    _ => break,
                }
            }
        }
    }
    risk
}

fn redirect(target: &str, ctx: &Ctx) -> Risk {
    if target == "/dev/null" || target == "/dev/stdout" || target == "/dev/stderr" {
        return Risk::safe();
    }
    let p = resolve(target, ctx);
    if is_device(&p) {
        return Risk::new(
            Level::Catastrophic,
            format!("writes straight to the device {}", p.display()),
        );
    }
    if let Some(why) = protected(&p, ctx) {
        return Risk::new(Level::Catastrophic, format!("overwrites {why}"));
    }
    Risk::low()
}

/// Commands that only read.
const SAFE: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "grep",
    "rg",
    "egrep",
    "fgrep",
    "echo",
    "printf",
    "pwd",
    "wc",
    "sort",
    "uniq",
    "diff",
    "cmp",
    "file",
    "stat",
    "du",
    "df",
    "which",
    "type",
    "whoami",
    "id",
    "date",
    "uname",
    "env",
    "printenv",
    "tree",
    "realpath",
    "dirname",
    "basename",
    "true",
    "false",
    "test",
    "[",
    "jq",
    "cut",
    "tr",
    "column",
    "nl",
    "od",
    "xxd",
    "hexdump",
    "sha256sum",
    "md5sum",
    "ps",
    "pgrep",
    "fd",
    "awk",
    "cd",
    "ss",
    "tokei",
    "cloc",
];
const GIT_SAFE: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
    "describe",
    "shortlog",
    "grep",
    "remote",
    "config",
    "reflog",
    "branch",
    "tag",
    "worktree",
    "stash",
    "ls-tree",
    "ls-remote",
    "cat-file",
    "for-each-ref",
    "show-ref",
    "rev-list",
    "merge-base",
    "name-rev",
    "count-objects",
    "check-ignore",
];
/// Prefix words that run the rest of the line as a command.
const WRAPPERS: &[&str] = &[
    "nohup", "time", "nice", "ionice", "command", "exec", "builtin", "unbuffer", "stdbuf",
];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh"];
/// Variables that make an otherwise read-only command run a program.
const RUNNING_VARS: &[&str] = &[
    "PAGER",
    "LESSOPEN",
    "LESSCLOSE",
    "BASH_ENV",
    "ENV",
    "PATH",
    "EDITOR",
    "VISUAL",
];
const SUDO_VALUE_FLAGS: &[&str] = &[
    "-u",
    "-g",
    "-h",
    "-p",
    "-C",
    "-D",
    "-r",
    "-t",
    "-U",
    "-T",
    "--user",
    "--group",
    "--host",
    "--prompt",
    "--chdir",
    "--role",
    "--type",
    "--other-user",
    "--close-from",
];
const XARGS_VALUE_FLAGS: &[&str] = &[
    "-n",
    "-L",
    "-P",
    "-s",
    "-I",
    "-d",
    "-E",
    "-a",
    "--max-args",
    "--max-procs",
    "--delimiter",
    "--arg-file",
];

/// The words after a wrapper's options: skips `-x`, `--x=…` and `--`, plus the
/// next word after a flag in `value_flags`.
fn skip_opts<'a>(args: &'a [String], value_flags: &[&str]) -> &'a [String] {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            return &args[i + 1..];
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        i += if value_flags.contains(&a.as_str()) { 2 } else { 1 };
    }
    &args[i.min(args.len())..]
}

/// GNU-style parse: short letters in `value_short` and long names in
/// `value_long` take a value (attached or the next word). Options may follow
/// positionals; `--` ends them.
struct Opts<'a> {
    pos: Vec<&'a str>,
    /// (letter or long name, value).
    opts: Vec<(String, Option<&'a str>)>,
}

impl Opts<'_> {
    fn has(&self, f: impl Fn(&str) -> bool) -> bool {
        self.opts.iter().any(|(k, _)| f(k))
    }
}

fn getopt<'a>(args: &'a [String], value_short: &str, value_long: &[&str]) -> Opts<'a> {
    let mut o = Opts {
        pos: Vec::new(),
        opts: Vec::new(),
    };
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        if a == "--" {
            o.pos.extend(it);
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            match long.split_once('=') {
                Some((k, v)) => o.opts.push((k.to_string(), Some(v))),
                None if value_long.contains(&long) => o.opts.push((long.to_string(), it.next())),
                None => o.opts.push((long.to_string(), None)),
            }
        } else if a.len() > 1 && a.starts_with('-') {
            for (j, c) in a.char_indices().skip(1) {
                if value_short.contains(c) {
                    let rest = &a[j + c.len_utf8()..];
                    let v = if rest.is_empty() { it.next() } else { Some(rest) };
                    o.opts.push((c.to_string(), v));
                    break;
                }
                o.opts.push((c.to_string(), None));
            }
        } else {
            o.pos.push(a);
        }
    }
    o
}

fn command(words: &[String], piped: bool, ctx: &Ctx, depth: u32) -> Risk {
    // Leading `VAR=value` assignments.
    let mut w: &[String] = words;
    let mut runs_var = false;
    while let Some(first) = w.first() {
        if first.contains('=') && !first.starts_with('-') && !first.starts_with('=') {
            let var = first.split('=').next().unwrap_or("");
            runs_var |= var.starts_with("GIT_") || var.starts_with("LD_") || RUNNING_VARS.contains(&var);
            w = &w[1..];
        } else {
            break;
        }
    }
    let r = simple(w, piped, ctx, depth);
    // `GIT_EXTERNAL_DIFF=… git diff`, `PAGER=… git log`: the variable runs a program.
    if runs_var && r.level == Level::Safe {
        Risk::low()
    } else {
        r
    }
}

/// A simple command without its leading assignments.
fn simple(w: &[String], piped: bool, ctx: &Ctx, depth: u32) -> Risk {
    let Some(first) = w.first() else {
        return Risk::safe();
    };
    let name = first.rsplit('/').next().unwrap_or(first).to_string();
    let args = &w[1..];
    let rest = || args.join(" ");
    match name.as_str() {
        "sudo" | "doas" | "pkexec" => {
            let inner = skip_opts(args, SUDO_VALUE_FLAGS);
            let inner_risk = if inner.is_empty() {
                Risk::safe()
            } else {
                command(inner, piped, ctx, depth)
            };
            Risk::new(Level::Confirm, format!("runs as root ({name})")).max(inner_risk)
        }
        "env" => {
            let mut i = 0;
            while let Some(a) = args.get(i) {
                let split = if a == "-S" || a == "--split-string" {
                    i += 1;
                    Some(args.get(i).map_or("", String::as_str))
                } else {
                    a.strip_prefix("--split-string=")
                        .or_else(|| a.strip_prefix("-S").filter(|s| !s.is_empty()))
                };
                if let Some(s) = split {
                    // `-S` splits this string into the command, and env appends the words after it.
                    let tail: Vec<String> = args[(i + 1).min(args.len())..]
                        .iter()
                        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
                        .collect();
                    let line = std::iter::once(s.to_string()).chain(tail).collect::<Vec<_>>().join(" ");
                    return classify_depth(&line, ctx, depth + 1);
                }
                if a == "--" {
                    i += 1;
                    break;
                }
                if !a.starts_with('-') {
                    break;
                }
                i += if ["-u", "-C", "--unset", "--chdir"].contains(&a.as_str()) {
                    2
                } else {
                    1
                };
            }
            // command() skips (and judges) the `VAR=value` words.
            let rest = &args[i.min(args.len())..];
            if rest.is_empty() {
                Risk::safe()
            } else {
                command(rest, piped, ctx, depth)
            }
        }
        "timeout" => {
            // Options, then the duration, then the command.
            let rest = skip_opts(args, &["-s", "-k", "--signal", "--kill-after"]);
            match rest.get(1..) {
                Some(c) if !c.is_empty() => command(c, piped, ctx, depth),
                _ => Risk::low(),
            }
        }
        "xargs" => {
            let rest = skip_opts(args, XARGS_VALUE_FLAGS);
            if rest.is_empty() {
                Risk::low()
            } else {
                command(rest, piped, ctx, depth).max(Risk::low())
            }
        }
        n if WRAPPERS.contains(&n) => {
            let value_flags: &[&str] = match n {
                "nice" => &["-n", "--adjustment"],
                "ionice" => &["-c", "-n", "-p", "-P", "-u"],
                "stdbuf" => &["-i", "-o", "-e"],
                "time" => &["-o", "-f", "--output", "--format"],
                "exec" => &["-a"],
                _ => &[],
            };
            let inner = skip_opts(args, value_flags);
            if inner.is_empty() {
                Risk::low()
            } else {
                command(inner, piped, ctx, depth)
            }
        }
        n if SHELLS.contains(&n) => {
            if let Some(i) = args
                .iter()
                .position(|a| a == "-c" || (a.starts_with('-') && a.ends_with('c')))
            {
                match args.get(i + 1) {
                    Some(script) => classify_depth(script, ctx, depth + 1).max(Risk::low()),
                    None => Risk::new(Level::Confirm, "shell -c without a script"),
                }
            } else if piped {
                Risk::new(Level::Confirm, "pipes downloaded or generated text into a shell")
            } else {
                Risk::low()
            }
        }
        "eval" => classify_depth(&rest(), ctx, depth + 1).max(Risk::low()),
        // Shell keywords: what follows them is the command (`$(…)` is checked on its own).
        "if" | "then" | "else" | "elif" | "while" | "until" | "do" | "!" => {
            if args.is_empty() {
                Risk::safe()
            } else {
                command(args, piped, ctx, depth)
            }
        }
        "for" | "done" | "fi" => Risk::safe(),
        "rm" => rm(args, ctx),
        "rmdir" | "unlink" => targets(args, ctx, "deletes").max(Risk::low()),
        "mv" | "cp" | "ln" | "install" => copy_like(&name, args, ctx),
        "rsync" => rsync(args, ctx),
        "chmod" | "chown" | "chgrp" | "setfacl" => {
            let base = targets(args, ctx, "changes permissions of");
            if has_flag(args, 'R', "--recursive") {
                base.max(Risk::new(Level::Confirm, format!("recursive {name}")))
            } else {
                base.max(Risk::low())
            }
        }
        "find" => {
            let roots: Vec<String> = args.iter().take_while(|a| !a.starts_with('-')).cloned().collect();
            let deletes = args.iter().any(|a| a == "-delete")
                || args.windows(2).any(|p| {
                    (p[0] == "-exec" || p[0] == "-execdir" || p[0] == "-ok")
                        && ["rm", "shred", "unlink"].contains(&p[1].rsplit('/').next().unwrap_or(&p[1]))
                });
            if deletes {
                targets(&roots, ctx, "deletes everything found under")
                    .max(Risk::new(Level::Confirm, "find deletes the files it matches"))
            } else if args
                .iter()
                .any(|a| a.starts_with("-exec") || a == "-ok" || a.starts_with("-fprint") || a == "-fls")
            {
                Risk::low()
            } else {
                Risk::safe()
            }
        }
        "shred" | "wipefs" | "mkswap" | "fdisk" | "sfdisk" | "parted" | "gdisk" | "blkdiscard" => {
            targets(args, ctx, "destroys").max(Risk::new(Level::Confirm, format!("{name} destroys data")))
        }
        n if n.starts_with("mkfs") => {
            targets(args, ctx, "formats").max(Risk::new(Level::Confirm, format!("{name} formats a filesystem")))
        }
        "truncate" => targets(args, ctx, "truncates").max(Risk::new(Level::Confirm, "truncate empties files")),
        "dd" => {
            let of = args.iter().find_map(|a| a.strip_prefix("of="));
            match of {
                Some(t) if is_device(&resolve(t, ctx)) => {
                    Risk::new(Level::Catastrophic, format!("dd writes straight to the device {t}"))
                }
                Some(t) => targets(&[t.to_string()], ctx, "overwrites")
                    .max(Risk::new(Level::Confirm, "dd overwrites its output")),
                None => Risk::low(),
            }
        }
        // `pkill -f` matches full command lines, and the shell running this command
        // has the pattern in its own: it kills itself (seen in a real session).
        "pkill"
            if has_flag(args, 'f', "--full")
                && args
                    .last()
                    .filter(|p| !p.starts_with('-'))
                    .is_some_and(|p| regex::Regex::new(p).is_ok_and(|r| r.is_match(p))) =>
        {
            Risk::new(
                Level::Catastrophic,
                "would kill its own shell: `pkill -f` matches full command lines and the pattern matches this command's text. Write the pattern so it can't match itself, e.g. `[g]it-receive-pack` instead of `git-receive-pack`",
            )
        }
        "kill" | "pkill" | "killall" => {
            if args.iter().any(|a| a == "-1" && name == "kill") && args.iter().filter(|a| *a == "-1").count() >= 1 {
                let all = args.last().is_some_and(|a| a == "-1");
                if all {
                    return Risk::new(Level::Catastrophic, "kill -1 signals every process you own");
                }
            }
            Risk::low()
        }
        "shutdown" | "reboot" | "poweroff" | "halt" => Risk::new(Level::Confirm, format!("{name} stops the machine")),
        "systemctl"
            if args
                .iter()
                .any(|a| ["poweroff", "reboot", "halt", "suspend"].contains(&a.as_str())) =>
        {
            Risk::new(Level::Confirm, "systemctl stops the machine")
        }
        "crontab" if args.iter().any(|a| a == "-r") => Risk::new(Level::Confirm, "crontab -r deletes every cron job"),
        "docker" | "podman" => {
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            let prune = a.contains(&"prune") || (a.contains(&"volume") && (a.contains(&"rm") || a.contains(&"remove")));
            if prune {
                Risk::new(Level::Confirm, format!("{name} deletes volumes or images"))
            } else {
                Risk::low()
            }
        }
        "git" => git(args),
        "sed" => sed(args),
        n if SAFE.contains(&n) => {
            if writes_or_runs(n, args) {
                Risk::low()
            } else {
                Risk::safe()
            }
        }
        _ => Risk::low(),
    }
}

/// Safe-list commands that write a file or run a program with these arguments.
fn writes_or_runs(name: &str, args: &[String]) -> bool {
    let short = |a: &str, c: char| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(c);
    let any = |f: &dyn Fn(&str) -> bool| args.iter().any(|a| f(a));
    static AWK_RUNS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"system\s*\(|getline|\|\s*"|"\s*\||print[f]?\b[^;}]*>"#).unwrap()
    });
    match name {
        "awk" => {
            let prog = &*AWK_RUNS;
            any(&|a| {
                (a.starts_with('-')
                    && !a.starts_with("--")
                    && (a.starts_with("-f") || a.starts_with("-i") || a.starts_with("-E")))
                    || a.starts_with("--include")
                    || a.starts_with("--file")
                    || a.starts_with("--exec")
                    || (!a.starts_with('-') && prog.is_match(a))
            })
        }
        "fd" => getopt(
            args,
            "teEdSjc",
            &["type", "extension", "exclude", "max-depth", "size", "threads", "color"],
        )
        .has(|k| k == "x" || k == "X" || k.starts_with("exec")),
        "sort" => getopt(
            args,
            "tkSTo",
            &["field-separator", "key", "buffer-size", "temporary-directory", "output"],
        )
        .has(|k| k == "o" || k == "output" || k == "compress-program"),
        "tree" => any(&|a| short(a, 'o')),
        "uniq" => {
            getopt(args, "fsw", &["skip-fields", "skip-chars", "check-chars"])
                .pos
                .len()
                >= 2
        }
        "xxd" => {
            getopt(args, "cglosn", &["cols", "groupsize", "len", "offset", "seek", "name"])
                .pos
                .len()
                >= 2
        }
        "rg" => any(&|a| a.starts_with("--pre")),
        "ss" => has_flag(args, 'K', "--kill"),
        "date" => {
            // Only `-s`/`--set` sets the clock; `-d`, `-f`, `-r` take a value, `-I` an attached one.
            let mut it = args.iter();
            while let Some(a) = it.next() {
                if a == "--" {
                    break;
                }
                if a.starts_with("--set") {
                    return true;
                }
                if ["--date", "--file", "--reference"].contains(&a.as_str()) {
                    it.next();
                }
                if a.starts_with("--") || !a.starts_with('-') {
                    continue;
                }
                for (j, c) in a.char_indices().skip(1) {
                    match c {
                        's' => return true,
                        'd' | 'f' | 'r' => {
                            if j + 1 == a.len() {
                                it.next();
                            }
                            break;
                        }
                        'I' => break,
                        _ => {}
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// An exact home or system folder (not a credential or user-protected path).
fn is_exact_dir(p: &Path, ctx: &Ctx) -> bool {
    p == ctx.home || SYSTEM_DIRS.iter().any(|d| p == Path::new(d))
}

/// Writing to `dest`: into an exact home or system folder (`into`, the file lands
/// inside it) is ordinary unless a source lands on a protected name there
/// (`cp -r x/.ssh ~`); replacing one, or writing under credentials, is not.
fn destination(dest: &str, sources: &[&str], ctx: &Ctx, into: bool, verb: &str) -> Risk {
    let p = resolve(dest, ctx);
    if is_device(&p) {
        return Risk::new(Level::Catastrophic, format!("{verb} the device {}", p.display()));
    }
    match protected(&p, ctx) {
        Some(_) if into && is_exact_dir(&p, ctx) => {
            let mut r = Risk::low();
            for s in sources.iter().filter(|s| !s.starts_with('-')) {
                let landed = resolve(s, ctx).file_name().map(|n| p.join(n));
                if let Some(why) = landed.and_then(|l| protected(&l, ctx)) {
                    r = r.max(Risk::new(Level::Catastrophic, format!("{verb} {why}")));
                }
            }
            r
        }
        Some(why) => Risk::new(Level::Catastrophic, format!("{verb} {why}")),
        None => Risk::low(),
    }
}

/// cp/ln/install/mv: the destination is at risk (the last positional, or the
/// `-t` folder); mv also takes its sources away.
fn copy_like(name: &str, args: &[String], ctx: &Ctx) -> Risk {
    let o = if name == "install" {
        getopt(args, "gmoSt", &["group", "mode", "owner", "suffix", "target-directory"])
    } else {
        getopt(args, "St", &["suffix", "target-directory"])
    };
    let target = o
        .opts
        .iter()
        .find(|(k, _)| k == "t" || k == "target-directory")
        .and_then(|(_, v)| *v);
    let (sources, dest): (&[&str], Option<&str>) = match (target, o.pos.split_last()) {
        (Some(t), _) => (&o.pos, Some(t)),
        (None, Some((d, s))) => (s, Some(*d)),
        (None, None) => (&[], None),
    };
    let into = !o.has(|k| k == "T" || k == "no-target-directory");
    let mut r = Risk::low();
    if name == "mv" {
        r = r.max(targets(sources, ctx, "moves"));
    }
    if let Some(d) = dest {
        r = r.max(destination(d, sources, ctx, into, "replaces"));
    }
    r
}

/// rsync: a home or system destination matters only with `--delete*`; sources
/// only with `--remove-source-files`.
fn rsync(args: &[String], ctx: &Ctx) -> Risk {
    let o = getopt(
        args,
        "eBfMT",
        &[
            "rsh",
            "filter",
            "exclude",
            "include",
            "exclude-from",
            "include-from",
            "files-from",
            "temp-dir",
            "partial-dir",
            "log-file",
            "chmod",
            "chown",
            "rsync-path",
            "link-dest",
            "compare-dest",
            "copy-dest",
            "backup-dir",
            "suffix",
            "password-file",
            "max-size",
            "min-size",
            "timeout",
            "bwlimit",
            "port",
            "out-format",
            "usermap",
            "groupmap",
            "max-delete",
        ],
    );
    let deletes = o.has(|k| k.starts_with("delete") || k == "del");
    let Some((dest, sources)) = o.pos.split_last() else {
        return Risk::low();
    };
    // `src/` copies the contents of src, not src itself, so its name doesn't land.
    // shortcut: what's inside such a source isn't known here; only a protected `src/` counts.
    let named: Vec<&str> = sources.iter().copied().filter(|s| !s.ends_with('/')).collect();
    let mut r = destination(
        dest,
        &named,
        ctx,
        !deletes,
        if deletes { "deletes files in" } else { "replaces" },
    );
    if !deletes && is_exact_dir(&resolve(dest, ctx), ctx) {
        let contents: Vec<&str> = sources.iter().copied().filter(|s| s.ends_with('/')).collect();
        r = r.max(targets(&contents, ctx, "copies out of"));
    }
    if o.has(|k| k == "remove-source-files") {
        r = r.max(targets(sources, ctx, "moves"));
    }
    r
}

fn has_flag(args: &[String], short: char, long: &str) -> bool {
    args.iter()
        .any(|a| a == long || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains(short)))
}

fn rm(args: &[String], ctx: &Ctx) -> Risk {
    let recursive = has_flag(args, 'r', "--recursive") || has_flag(args, 'R', "--recursive");
    let force = has_flag(args, 'f', "--force");
    let base = targets(args, ctx, "deletes");
    if recursive || force {
        let what = if recursive {
            "deletes folders recursively"
        } else {
            "force-deletes files"
        };
        base.max(Risk::new(Level::Confirm, format!("rm {what}")))
    } else {
        base.max(Risk::low())
    }
}

/// Catastrophic when any non-flag argument resolves to a protected place.
fn targets<S: AsRef<str>>(args: &[S], ctx: &Ctx, verb: &str) -> Risk {
    let mut r = Risk::safe();
    for a in args.iter().map(AsRef::as_ref).filter(|a| !a.starts_with('-')) {
        let p = resolve(a, ctx);
        if is_device(&p) {
            r = r.max(Risk::new(
                Level::Catastrophic,
                format!("{verb} the device {}", p.display()),
            ));
        } else if let Some(why) = protected(&p, ctx) {
            r = r.max(Risk::new(Level::Catastrophic, format!("{verb} {why}")));
        }
    }
    r
}

/// `sed` only reads unless it edits in place, runs a script file, or its script
/// writes (`w`, `W`) or executes (`e`). A `w`/`e` the check can't rule out counts.
fn sed(args: &[String]) -> Risk {
    let short = |a: &String, c: char| a.starts_with('-') && !a.starts_with("--") && a.contains(c);
    let in_place = args.iter().any(|a| a.starts_with("--in-place") || short(a, 'i'));
    let script_file = args.iter().any(|a| a.starts_with("--file") || short(a, 'f'));
    let writes = regex::Regex::new(r"(^|[/;{}0-9$p]\s*)[wWe](\s|;|$)").unwrap();
    if in_place || script_file || args.iter().any(|a| !a.starts_with('-') && writes.is_match(a)) {
        Risk::low()
    } else {
        Risk::safe()
    }
}

fn git(args: &[String]) -> Risk {
    // Config keys that can't run a program (`core.fsmonitor`, `alias.x=!…`, `core.pager` can).
    let harmless = |kv: &str| {
        let k = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
        ["color.", "core.quotepath", "safe.directory", "user.", "advice."]
            .iter()
            .any(|p| k.starts_with(p))
    };
    // Skip global options: -C <dir>, -c <k=v>, --git-dir=…
    let mut i = 0;
    let mut runs = false;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-C" => i += 2,
            "-c" | "--config-env" => {
                runs |= !args.get(i + 1).is_some_and(|kv| harmless(kv));
                i += 2;
            }
            a if a.starts_with("--config-env=") => {
                runs |= !harmless(&a["--config-env=".len()..]);
                i += 1;
            }
            a if a.starts_with("--exec-path") => {
                runs = true;
                i += 1;
            }
            a if a.starts_with('-') => i += 1,
            _ => break,
        }
    }
    let r = git_sub(args, i);
    let rest = args.get(i + 1..).unwrap_or_default();
    // Options that write a file or run a program even under a read-only subcommand.
    runs |= rest.iter().any(|r| {
        r.starts_with("--output")
            || r.starts_with("-O")
            || r.starts_with("--open-files-in-pager")
            || r == "--ext-diff"
            || r.starts_with("--upload-pack")
            || r.starts_with("ext::")
    }) || (args.get(i).is_some_and(|s| s == "ls-remote") && rest.iter().any(|r| r == "-u"));
    if runs { r.max(Risk::low()) } else { r }
}

/// The subcommand at `args[i]`, without the global options.
fn git_sub(args: &[String], i: usize) -> Risk {
    let Some(sub) = args.get(i) else { return Risk::safe() };
    let rest: Vec<&str> = args[i + 1..].iter().map(String::as_str).collect();
    let has = |x: &str| rest.contains(&x);
    let confirm = |why: &str| Risk::new(Level::Confirm, why);
    match sub.as_str() {
        "push"
            if rest.iter().any(|r| {
                r.starts_with("--force")
                    || r.starts_with('+')
                    || (r.starts_with('-') && !r.starts_with("--") && r.contains('f'))
            }) =>
        {
            Risk::new(Level::Shared, "git push --force rewrites the remote's history")
        }
        "push" if has("--mirror") || rest.iter().any(|r| r.starts_with("--prune")) => {
            Risk::new(Level::Shared, "git push --mirror/--prune deletes remote branches")
        }
        "push" if has("--delete") || has("-d") || rest.iter().any(|r| r.starts_with(':')) => {
            Risk::new(Level::Shared, "git push deletes a remote branch")
        }
        "reset" if has("--hard") => confirm("git reset --hard discards uncommitted changes"),
        "clean"
            if rest
                .iter()
                .any(|r| r.starts_with('-') && !r.starts_with("--") && r.contains('f'))
                || has("--force") =>
        {
            confirm("git clean deletes untracked files")
        }
        "checkout" if has("--") || has(".") || has("-f") => confirm("git checkout discards uncommitted changes"),
        "restore" if !has("--staged") || has("--worktree") => confirm("git restore discards uncommitted changes"),
        "branch" if has("-D") || (has("-d") && has("--force")) => confirm("git branch -D deletes an unmerged branch"),
        "stash" if has("drop") || has("clear") => confirm("git stash drop/clear deletes stashed work"),
        // Bare `git stash` (and `git stash -u`) changes the working tree.
        "stash" if !has("list") && !has("show") => Risk::low(),
        "reflog" if has("expire") || has("delete") => confirm("git reflog expire/delete removes history entries"),
        "filter-branch" | "filter-repo" => confirm("rewrites the whole repository history"),
        "worktree" if has("remove") && (has("-f") || has("--force")) => confirm("removes a worktree with its changes"),
        "branch" | "tag" | "stash" | "remote" | "config" | "worktree"
            if rest.iter().any(|r| !r.starts_with('-')) && !has("list") =>
        {
            Risk::low()
        }
        s if GIT_SAFE.contains(&s) => Risk::safe(),
        _ => Risk::low(),
    }
}

/// Expand `~` and `$HOME`, resolve against the cwd, normalise `.`/`..`, and
/// strip a trailing `/*` or `/` (deleting `dir/*` is as bad as deleting `dir`).
fn resolve(arg: &str, ctx: &Ctx) -> PathBuf {
    let home = ctx.home.to_string_lossy();
    let mut s = arg.to_string();
    for var in ["${HOME}", "$HOME"] {
        if let Some(rest) = s.strip_prefix(var) {
            s = format!("{home}{rest}");
        }
    }
    if s == "~" || s.starts_with("~/") {
        s = format!("{home}{}", &s[1..]);
    }
    while s.len() > 1 && (s.ends_with("/*") || s.ends_with("/.*") || s.ends_with('/')) {
        s = s
            .trim_end_matches("/*")
            .trim_end_matches("/.*")
            .trim_end_matches('/')
            .to_string();
        if s.is_empty() {
            s = "/".into();
        }
    }
    if s == "*" || s == ".*" {
        s = ".".into();
    }
    let p = Path::new(&s);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.cwd.join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

const SYSTEM_DIRS: &[&str] = &[
    "/", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib64", "/opt", "/proc", "/root", "/sbin", "/srv", "/sys",
    "/usr", "/var",
];
const CREDENTIAL_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".config/gcloud",
    ".password-store",
    ".novacode",
];

/// Why writing a file at `path` would touch a protected place, or `None`: the
/// file tools refuse what a catastrophic shell command would (L6.2).
/// Under `~/.novacode` only the login and trust files count, so the agent can
/// still edit AGENTS.md, skills and settings there.
pub fn protected_path(path: &Path, ctx: &Ctx) -> Option<String> {
    let p = resolve(&path.to_string_lossy(), ctx);
    let nova = ctx.home.join(".novacode");
    if let Ok(rest) = p.strip_prefix(&nova)
        && !rest.as_os_str().is_empty()
    {
        let mut parts = rest.components();
        let first = parts.next().map(|c| c.as_os_str().to_string_lossy().into_owned());
        let cred = first.as_deref().is_some_and(|n| {
            n == "auth.json"
                || n == "mcp-auth.json"
                || n == "trust.json"
                || (n.starts_with("auth-") && n.ends_with(".json"))
        });
        if !(cred && parts.next().is_none()) {
            return extra_protected(&p, ctx);
        }
    }
    protected(&p, ctx)
}

/// Why `p` is protected, or `None`.
fn protected(p: &Path, ctx: &Ctx) -> Option<String> {
    if SYSTEM_DIRS.iter().any(|d| p == Path::new(d)) {
        return Some(format!("the system folder {}", p.display()));
    }
    if p == ctx.home {
        return Some("your home folder".into());
    }
    for d in CREDENTIAL_DIRS {
        let dir = ctx.home.join(d);
        if p == dir || p.starts_with(&dir) {
            return Some(format!("credentials in ~/{d}"));
        }
    }
    extra_protected(p, ctx)
}

/// The user's own protected paths from settings.
fn extra_protected(p: &Path, ctx: &Ctx) -> Option<String> {
    for extra in ctx.protected {
        let e = resolve(extra, ctx);
        if p == e || p.starts_with(&e) {
            return Some(format!("the protected path {extra}"));
        }
    }
    None
}

fn is_device(p: &Path) -> bool {
    let s = p.to_string_lossy();
    [
        "/dev/sd",
        "/dev/nvme",
        "/dev/hd",
        "/dev/vd",
        "/dev/xvd",
        "/dev/mmcblk",
        "/dev/disk",
        "/dev/mapper/",
    ]
    .iter()
    .any(|d| s.starts_with(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Level::*;

    fn level(cmd: &str) -> Level {
        let protected = vec!["~/secrets".to_string()];
        let ctx = Ctx {
            cwd: Path::new("/home/u/proj"),
            home: Path::new("/home/u"),
            protected: &protected,
        };
        classify(cmd, &ctx).level
    }

    #[test]
    fn table() {
        let cases: &[(&str, Level)] = &[
            // Read-only
            ("ls -la", Safe),
            ("cat a.txt | grep x | wc -l", Safe),
            ("git status && git diff HEAD~1", Safe),
            ("rg foo src/ 2>/dev/null", Safe),
            ("echo hi >&2", Safe),
            ("find . -name '*.rs'", Safe),
            ("git branch", Safe),
            // Plan-mode refusals seen in real sessions: these only read.
            ("cd /r && git show origin/main:a.tsx | sed -n 586,720p", Safe),
            (
                "git ls-tree -r -l origin/main data | sed 's#/[^/]*$##' | sort | uniq -c",
                Safe,
            ),
            ("git -C /r ls-remote origin refs/heads/main", Safe),
            (
                "for r in $(git for-each-ref --format='%(refname:short)' refs/heads); do git grep -l x $r; done",
                Safe,
            ),
            (
                "git cat-file -p HEAD && git merge-base HEAD main && git rev-list --count HEAD",
                Safe,
            ),
            ("sed 's/-[0-9].*//;s/[.]json//' a.txt", Safe),
            // …but sed that edits, writes or executes doesn't.
            ("sed -i 's/a/b/' f.txt", Low),
            ("sed -ni 's/a/b/p' f.txt", Low),
            ("sed --in-place=.bak 's/a/b/' f.txt", Low),
            ("sed 's/a/b/w out.txt' f.txt", Low),
            ("sed -n '1,5w out.txt' f.txt", Low),
            ("sed 's/.*/date/e' f.txt", Low),
            ("sed -f prog.sed f.txt", Low),
            ("for f in a b; do rm -rf ~; done", Catastrophic),
            ("for f in $(rm -rf ~); do echo $f; done", Catastrophic),
            ("if true; then git push --force; fi", Shared),
            // Heredoc bodies are data (the screenshot case: JS with `=>` and `{`).
            (
                "cat > ./q.mjs <<'EOF'\nconst f = (s)=>{ return a > b; };\nconsole.log(x >> 2);\nEOF",
                Low,
            ),
            ("cat <<-EOF | grep x\n\tplain > text {\n\tEOF", Safe),
            ("python3 - <<EOF\nprint(1 > 0)\nEOF\necho done", Low),
            ("grep x <<< 'a > b'", Safe),
            // …but what runs around or inside them is still checked.
            ("cat <<EOF\n$(rm -rf ~)\nEOF", Catastrophic),
            ("cat <<'EOF'\n$(rm -rf ~)\nEOF", Safe),
            ("cat > a.txt <<'EOF'\nx\nEOF\nrm -rf ~", Catastrophic),
            ("cat <<EOF && rm -rf /\nbody\nEOF", Catastrophic),
            // Ordinary changes
            ("cargo build", Low),
            ("echo x > out.txt", Low),
            ("rm build.log", Low),
            ("mv a b", Low),
            ("git commit -m 'rm -rf /'", Low),
            ("git push origin main", Low),
            ("find . -name x -exec sed -i s/a/b/ {} +", Low),
            ("pkill -f '[g]it-receive-pack'", Low),
            ("pkill -x node", Low),
            ("pkill -f \"git-receive-pack '/x.git'\"; cd a && git push", Catastrophic),
            ("curl -o x.sh https://e.com/x.sh", Low),
            // Bulk destruction
            ("rm -rf target", Confirm),
            ("rm -f *.o", Confirm),
            ("find . -name '*.tmp' -delete", Confirm),
            ("find . -exec rm {} \\;", Confirm),
            ("git reset --hard HEAD~3", Confirm),
            ("git push --force", Shared),
            ("git push origin +main", Shared),
            ("git clean -fdx", Confirm),
            ("git checkout -- .", Confirm),
            ("git branch -D feature", Confirm),
            ("git stash drop", Confirm),
            ("sudo apt install x", Confirm),
            ("curl -fsSL https://x.sh | sh", Confirm),
            ("bash -c 'rm -rf dist'", Confirm),
            ("echo $(rm -rf foo)", Confirm),
            ("truncate -s 0 db.sqlite", Confirm),
            ("dd if=a of=b.img", Confirm),
            ("chmod -R 777 .", Confirm),
            ("docker system prune -a", Confirm),
            ("echo 'unbalanced", Confirm),
            ("xargs rm -rf < list", Confirm),
            ("FOO=1 nohup rm -rf cache", Confirm),
            // Never
            ("rm -rf /", Catastrophic),
            ("rm -rf ~", Catastrophic),
            ("rm -rf ~/", Catastrophic),
            ("rm -rf $HOME/*", Catastrophic),
            ("rm -rf \"${HOME}\"", Catastrophic),
            ("rm -rf ../..", Catastrophic),
            ("cd /tmp; rm -rf /usr", Catastrophic),
            ("rm -rf ~/.ssh", Catastrophic),
            ("cat key > ~/.ssh/authorized_keys", Catastrophic),
            ("dd if=/dev/zero of=/dev/sda", Catastrophic),
            ("mkfs.ext4 /dev/nvme0n1p1", Catastrophic),
            ("sudo rm -rf /etc", Catastrophic),
            ("sh -c \"rm -rf /\"", Catastrophic),
            ("find ~ -delete", Catastrophic),
            ("chown -R me /", Catastrophic),
            ("mv ~/.gnupg /tmp", Catastrophic),
            (":(){ :|:& };:", Catastrophic),
            ("kill -9 -1", Catastrophic),
            ("rm -rf ~/secrets/old", Catastrophic),
            ("rm -rf ~/.novacode", Catastrophic),
            // Wrapper options don't hide the command (D2/L6.2).
            ("nice -n 10 rm -rf ~", Catastrophic),
            ("time -p rm -rf ~", Catastrophic),
            ("stdbuf -oL rm -rf ~", Catastrophic),
            ("ionice -c3 rm -rf ~", Catastrophic),
            ("command -p rm -rf ~", Catastrophic),
            ("exec -a foo rm -rf ~", Catastrophic),
            ("nohup -- rm -rf ~", Catastrophic),
            ("timeout -s KILL 60 rm -rf ~", Catastrophic),
            ("xargs -n 1 rm -rf ~", Catastrophic),
            ("env -u X rm -rf ~", Catastrophic),
            ("env -C /tmp rm -rf ~", Catastrophic),
            ("env -S 'rm -rf ~'", Catastrophic),
            ("env -S rm -rf ~", Catastrophic),
            ("env -S 'rm -rf' ~", Catastrophic),
            ("env --split-string=rm -rf ~", Catastrophic),
            ("env -S 'echo' hi", Safe),
            ("sudo -u root rm -rf ~", Catastrophic),
            ("sudo --group wheel rm -rf ~", Catastrophic),
            ("sudo --user root --chdir /tmp rm -rf ~", Catastrophic),
            ("time -p git status", Safe),
            // git options and variables that run programs.
            ("git -c core.fsmonitor='touch x' status", Low),
            ("git -c alias.x='!touch y' x", Low),
            ("git --config-env=core.pager=P log", Low),
            ("GIT_EXTERNAL_DIFF=./x git diff", Low),
            ("env GIT_EXTERNAL_DIFF=./x git diff", Low),
            ("PAGER=./x git log", Low),
            ("git diff --output=f", Low),
            ("git grep -Ovim x", Low),
            ("git ls-remote --upload-pack='touch z' origin", Low),
            ("git ls-remote -u 'touch z' origin", Low),
            ("git diff --ext-diff", Low),
            ("git ls-remote 'ext::sh -c touch% x'", Low),
            ("git stash", Low),
            ("git stash -u", Low),
            ("git stash list", Safe),
            ("git reflog expire --all", Confirm),
            ("git reflog delete HEAD@{1}", Confirm),
            ("git -c color.ui=always log", Safe),
            ("LC_ALL=C sort a", Safe),
            // Safe-list commands that write or run.
            ("awk 'BEGIN{system(\"x\")}'", Low),
            ("awk '{print > \"f\"}' a", Low),
            ("awk -i inplace '{print}' f", Low),
            ("awk '$3 > 100' f", Safe),
            ("fd -x rm {}", Low),
            ("fd -tx rs", Safe),
            ("fd -Hx rm", Low),
            ("fd -X rm", Low),
            ("fd --exec-batch rm", Low),
            ("fd -e x rs", Safe),
            ("find . -fprint out", Low),
            ("sort -o out a", Low),
            ("sort -ro out a", Low),
            ("sort --output=out a", Low),
            ("sort -to a", Safe),
            ("sort -t o -k 2 a", Safe),
            ("sort --compress-program=x a", Low),
            ("tree -o out", Low),
            ("uniq a out", Low),
            ("uniq -c a", Safe),
            ("xxd a out", Low),
            ("rg --pre ./p x", Low),
            ("ss -K dst 1.2.3.4", Low),
            ("date -s 2020-01-01", Low),
            ("date +%s", Safe),
            ("date -Iseconds", Safe),
            ("date -us 2020-01-01", Low),
            ("date --set=2020-01-01", Low),
            ("date -d 'last sun' +%s", Safe),
            ("date -d -s", Safe),
            // Any force or branch-deleting push.
            ("git push --force-with-lease", Shared),
            ("git push --force-if-includes", Shared),
            ("git push -uf origin x", Shared),
            ("git push --mirror", Shared),
            ("git push --prune origin", Shared),
            ("git push -u origin x", Low),
            // Copying into home is fine; replacing it or credentials is not.
            ("cp file.txt ~/", Low),
            ("cp file.txt ~", Low),
            ("mv a ~/", Low),
            ("ln -s x ~", Low),
            ("cp ~/.ssh/id_ed25519.pub .", Low),
            ("cp -t ~ a b", Low),
            ("cp -T dir ~", Catastrophic),
            ("rsync -a --delete src/ ~/", Catastrophic),
            ("rsync -a src/ ~/", Low),
            // Copying a credential-named source into home merges into the credentials.
            ("cp -r payload/.ssh ~", Catastrophic),
            ("cp -r .novacode ~/", Catastrophic),
            ("cp -t ~ .ssh", Catastrophic),
            ("rsync -a dir/.ssh ~/", Catastrophic),
            ("rsync -a ~/.ssh/ ~/", Catastrophic),
            ("mv x/.gnupg ~", Catastrophic),
            ("rsync -a --remove-source-files ~/.ssh/ /tmp/b", Catastrophic),
            ("cp k ~/.ssh/authorized_keys", Catastrophic),
            ("cp -t ~/.ssh k", Catastrophic),
            ("install -m 600 k ~/.ssh/id_rsa", Catastrophic),
            ("cp k ~/secrets", Catastrophic),
        ];
        let mut bad = Vec::new();
        for (cmd, want) in cases {
            let got = level(cmd);
            if got != *want {
                bad.push(format!("{cmd:?}: want {want:?}, got {got:?}"));
            }
        }
        assert!(bad.is_empty(), "\n{}", bad.join("\n"));
    }

    #[test]
    fn heredoc_script_from_the_field_is_not_confirm() {
        // A real card the user hit: JS in a quoted heredoc read as shell redirects.
        let cmd = r#"cd /home/user/Projects/app && cat > ./qpeek.mjs <<'EOF'
import Redis from "ioredis";
const u = process.env.SPEECH_REDIS_URL || process.env.REDIS_URL;
console.log("using", process.env.SPEECH_REDIS_URL ? "SPEECH_REDIS_URL":"REDIS_URL", "host:", new URL(u).hostname);
const r = new Redis(u, { connectTimeout: 5000, maxRetriesPerRequest: 2 });
for (const q of ["speech:jobs","pron:jobs"]) {
  const items = await r.lrange(q, 0, -1);
  const rows = items.map(s=>{ const j=JSON.parse(s); const ts=parseInt(String(j.id).replace(/^t-/,"").split("-")[0],36); return [new Date(ts).toISOString(), j.engine ?? "-", j.id]; });
  console.log(q, rows.length, "oldest", rows[0], "newest", rows[rows.length-1]);
  console.log(" engines:", rows.reduce((a,x)=>(a[x[1]]=(a[x[1]]||0)+1,a),{}));
}
await r.quit();
EOF
node ./qpeek.mjs"#;
        let home = Path::new("/home/user");
        let ctx = Ctx {
            cwd: home,
            home,
            protected: &[],
        };
        let r = classify(cmd, &ctx);
        assert!(r.level < Level::Confirm, "{r:?}");
        assert_eq!(segments(cmd).unwrap().len(), 3, "{:?}", segments(cmd));
    }

    #[test]
    fn segments_split_compound_commands() {
        assert_eq!(
            segments("git add . && git commit -m 'a; b' | tee log").unwrap(),
            vec!["git add .", "git commit -m a; b", "tee log"]
        );
        assert!(segments("echo 'x").is_none());
    }
}
