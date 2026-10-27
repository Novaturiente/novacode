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
    /// A heredoc body, at the place of its `<<DELIM` (L6.2: inline code fed to an interpreter).
    Heredoc(String),
    /// `<<<`: the next word is fed on stdin.
    HereString,
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
    // Heredocs opened on this line: (delimiter, strip leading tabs, delimiter quoted,
    // index of its `Tok::Heredoc`).
    let mut heredocs: Vec<(String, bool, bool, usize)> = Vec::new();
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
                        // As bash: inside "…" a `\` escapes only `$`, `` ` ``, `"`, `\` and a newline;
                        // before anything else it stays (`"C:\Users"` keeps its `\`).
                        '\\' => match *c.get(i + 1)? {
                            '\n' => i += 1,
                            n @ ('$' | '`' | '"' | '\\') => {
                                i += 1;
                                word.push(n);
                            }
                            _ => word.push('\\'),
                        },
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
                    for (delim, strip, quoted, at) in std::mem::take(&mut heredocs) {
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
                        toks[at] = Tok::Heredoc(body);
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
            '<' if c.get(i + 1) == Some(&'<') && c.get(i + 2) == Some(&'<') => {
                flush(&mut word, &mut in_word, &mut toks);
                toks.push(Tok::HereString);
                i += 3;
                continue;
            }
            '<' if c.get(i + 1) == Some(&'<') => {
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
                heredocs.push((delim, strip, quoted, toks.len()));
                toks.push(Tok::Heredoc(String::new()));
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
            Tok::Heredoc(_) | Tok::HereString => {}
        }
    }
    if !cur.is_empty() {
        out.push(cur.join(" "));
    }
    Some(out)
}

/// A simple command of a line, for the command judge (L6.8) and allow rules (L6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Simple {
    /// Its words, without leading `VAR=value` words, shell keywords and wrappers.
    pub words: Vec<String>,
    /// Text reaches its stdin (a pipe, heredoc or here-string).
    pub stdin: bool,
    /// Where its `>`/`>>` redirects write.
    pub targets: Vec<String>,
    /// It runs inside another command (`$(…)`, `sh -c`, `eval`, a heredoc fed to a
    /// shell, `find -exec`, `watch`, …), not in place of the line's own words.
    pub nested: bool,
    /// Its `VAR=value` words, also those after `env` or `sudo`.
    pub env: Vec<String>,
    /// Assignments or wrappers (`xargs`, `env`, `timeout`, `sudo`, …) were stripped:
    /// `words` may not be all it runs with (`xargs` adds stdin's words).
    pub wrapped: bool,
}

/// The simple commands of a line for the command judge (L6.8): wrappers (`sudo`,
/// `env`, `timeout`, `xargs`, `nohup`, …) and shell keywords are stripped, and
/// what runners (`watch`, `setsid`, `find -exec`, `su -c`, …), `$(…)` bodies,
/// `sh -c`/`eval` scripts and heredocs fed to a shell run count as commands of
/// their own. `None` = it can't be parsed or nests too deeply.
pub(crate) fn commands(src: &str) -> Option<Vec<Simple>> {
    let mut out = Vec::new();
    commands_into(src, 0, false, &mut out)?;
    Some(out)
}

/// The commands of `src` that run inside another one, as text: an allow rule
/// that covers the line's own commands must cover these too (L6.1).
/// `None` = it can't be parsed.
pub fn nested(src: &str) -> Option<Vec<String>> {
    Some(
        commands(src)?
            .into_iter()
            .filter(|s| s.nested && !s.words.is_empty())
            .map(|s| s.words.join(" "))
            .collect(),
    )
}

/// Every command of `src` the parser finds, as text, with wrappers, keywords
/// and assignments removed and the program as its base name: deny and ask
/// rules match these (`timeout 5 /usr/bin/curl x` hits `Bash(curl:*)`, L6.1).
/// `None` = it can't be parsed.
pub fn programs(src: &str) -> Option<Vec<String>> {
    Some(
        commands(src)?
            .into_iter()
            .filter_map(|s| {
                let (first, rest) = s.words.split_first()?;
                let base = cmd_name(first);
                // git's global options don't change the subcommand: `git -C . push` is a `git push`.
                let rest = if base == "git" { git_globals(rest) } else { rest };
                Some(
                    std::iter::once(base.as_ref())
                        .chain(rest.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            })
            .collect(),
    )
}

/// `args` of git without its leading global options, for deny and ask rules
/// and the judge (L6.1): `-C`, `-c`, `--git-dir`, `--work-tree`, `--namespace`
/// with their values (also attached: `-C.`, `-ck=v`, `--git-dir=x`), the
/// `--opt=value` ones (`--exec-path`, `--config-env`, `--attr-source`,
/// `--super-prefix`, `--list-cmds`) and the plain flags (`-p`, `-P`,
/// `--no-pager`, `--bare`, `--no-optional-locks`, the pathspec flags, …).
pub(crate) fn git_globals(args: &[String]) -> &[String] {
    const VALUE_EQ: &[&str] = &[
        "--git-dir=",
        "--work-tree=",
        "--namespace=",
        "--exec-path=",
        "--config-env=",
        "--attr-source=",
        "--super-prefix=",
        "--list-cmds=",
    ];
    let mut i = 0;
    while let Some(a) = args.get(i) {
        i += match a.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => 2,
            "-p"
            | "-P"
            | "--paginate"
            | "--no-pager"
            | "--bare"
            | "--exec-path"
            | "--no-optional-locks"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-replace-objects"
            | "--no-lazy-fetch"
            | "--no-advice" => 1,
            a if a.starts_with("-C") || a.starts_with("-c") => 1,
            a if VALUE_EQ.iter().any(|p| a.starts_with(p)) => 1,
            _ => break,
        };
    }
    &args[i.min(args.len())..]
}

/// A command name built at run time (`$x`, `$(…)`, backticks): nobody can tell
/// from the text what runs (L6.2, L6.8).
pub(crate) fn computed(words: &[String]) -> bool {
    words
        .first()
        .is_some_and(|f| f.is_empty() || f.contains('$') || f.contains('`'))
}

/// `command -v`/`-V` only looks a name up; it doesn't run it.
fn lookup_only(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.starts_with('-') && *a != "-" && *a != "--")
        .any(|a| a.contains('v') || a.contains('V'))
}

fn commands_into(src: &str, depth: u32, nested: bool, out: &mut Vec<Simple>) -> Option<()> {
    if depth > 4 {
        return None;
    }
    let (toks, subs) = lex(src)?;
    for s in &subs {
        commands_into(s, depth + 1, true, out)?;
    }
    let (mut words, mut stdin, mut piped) = (Vec::new(), false, false);
    let (mut input, mut targets) = (Vec::new(), Vec::new());
    let mut iter = toks.into_iter().peekable();
    loop {
        match iter.next() {
            Some(Tok::Word(w)) => words.push(w),
            Some(Tok::Heredoc(body)) => {
                stdin = true;
                input.push(body);
            }
            Some(Tok::HereString) => {
                stdin = true;
                if let Some(Tok::Word(w)) = iter.peek() {
                    input.push(w.clone());
                    iter.next();
                }
            }
            Some(Tok::Redirect) => {
                if let Some(Tok::Word(t)) = iter.peek() {
                    targets.push(t.clone());
                    iter.next();
                }
            }
            sep @ (Some(Tok::Sep(_)) | None) => {
                let at = Here {
                    stdin: stdin || piped,
                    input: &input,
                    depth,
                    nested,
                };
                simple_into(&words, &at, std::mem::take(&mut targets), out)?;
                words.clear();
                input.clear();
                stdin = false;
                match sep {
                    Some(Tok::Sep(s)) => piped = s == "|",
                    _ => break,
                }
            }
        }
    }
    Some(())
}

/// Where a simple command sits, for [`simple_into`].
struct Here<'a> {
    stdin: bool,
    /// Heredoc bodies and here-strings fed to it.
    input: &'a [String],
    depth: u32,
    nested: bool,
}

fn simple_into(words: &[String], at: &Here, targets: Vec<String>, out: &mut Vec<Simple>) -> Option<()> {
    let push = |out: &mut Vec<Simple>, words: &[String], targets: Vec<String>, nested: bool| {
        if !words.is_empty() || !targets.is_empty() {
            out.push(Simple {
                words: words.to_vec(),
                stdin: at.stdin,
                targets,
                nested,
                env: Vec::new(),
                wrapped: false,
            });
        }
    };
    let Some(kw) = keywords(words) else {
        push(out, &[], targets, at.nested);
        return Some(());
    };
    let w = match unwrap(kw) {
        Inner::Words(w) => w,
        Inner::Script(s) => {
            push(out, &[], targets, at.nested);
            return commands_into(&s, at.depth + 1, true, out);
        }
    };
    let Some(first) = w.first() else {
        push(out, &[], targets, at.nested);
        return Some(());
    };
    let name = cmd_name(first);
    let name = name.as_ref();
    let args = &w[1..];
    let deeper = at.depth + 1;
    if SHELLS.contains(&name) {
        let (c, s, script) = shell_args(args);
        if let (true, Some(script)) = (c, script) {
            push(out, &[], targets, at.nested);
            return commands_into(script, deeper, true, out);
        }
        if !c && (s || script.is_none_or(|f| f == "-")) {
            for body in at.input {
                commands_into(body, deeper, true, out)?;
            }
        }
    } else if name == "eval" {
        push(out, &[], targets, at.nested);
        return commands_into(&args.join(" "), deeper, true, out);
    }
    for inner in runner(name, args) {
        match inner {
            Inner::Words(iw) => {
                let inner_at = Here { nested: true, ..*at };
                simple_into(iw, &inner_at, Vec::new(), out)?;
            }
            Inner::Script(s) => commands_into(&s, deeper, true, out)?,
        }
    }
    if !w.is_empty() || !targets.is_empty() {
        let env = kw[..kw.len() - w.len()]
            .iter()
            .filter(|a| a.contains('=') && !a.starts_with('-') && !a.starts_with('='))
            .cloned()
            .collect();
        out.push(Simple {
            words: w.to_vec(),
            stdin: at.stdin,
            targets,
            nested: at.nested,
            env,
            wrapped: kw.len() != w.len(),
        });
    }
    Some(())
}

/// `words` without leading shell keywords; `None` when they aren't a command
/// (`for x in a b`). A `case x in` head and a `pattern)` are skipped too.
fn keywords(words: &[String]) -> Option<&[String]> {
    let mut w = words;
    loop {
        match w.first().map(String::as_str) {
            Some(
                "if" | "then" | "else" | "elif" | "fi" | "while" | "until" | "do" | "done" | "esac" | "in" | "!"
                | "coproc",
            ) => w = &w[1..],
            Some("for" | "select") => return None,
            Some("case") => w = &w[w.iter().position(|a| a == "in").map_or(w.len(), |i| i + 1)..],
            Some(p) if p.ends_with(')') && !p.contains("$(") => w = &w[1..],
            _ => return Some(w),
        }
    }
}

/// What a wrapper or runner runs: words of a command, or a script to parse.
enum Inner<'a> {
    Words(&'a [String]),
    Script(String),
}

/// `words` without leading assignments and wrappers (with their options);
/// `env -S` gives its split string as a script.
fn unwrap(words: &[String]) -> Inner<'_> {
    let mut w = words;
    loop {
        while w
            .first()
            .is_some_and(|f| f.contains('=') && !f.starts_with('-') && !f.starts_with('='))
        {
            w = &w[1..];
        }
        let Some(first) = w.first() else { return Inner::Words(w) };
        let name = cmd_name(first);
        let args = &w[1..];
        w = match name.as_ref() {
            "sudo" | "doas" | "pkexec" => skip_opts(args, SUDO_VALUE_FLAGS),
            "env" => match env_split(args) {
                Ok(line) => return Inner::Script(line),
                Err(i) => &args[i..],
            },
            "timeout" => skip_opts(args, &["-s", "-k", "--signal", "--kill-after"])
                .get(1..)
                .unwrap_or_default(),
            "xargs" => skip_opts(args, XARGS_VALUE_FLAGS),
            "command" if lookup_only(args) => return Inner::Words(w),
            n if WRAPPERS.contains(&n) => skip_opts(args, wrapper_flags(n)),
            _ => return Inner::Words(w),
        };
    }
}

/// `env`'s arguments: `Ok(line)` for `-S`/`--split-string` (the string plus the
/// words after it, which env appends), else `Err(index of the command)`.
fn env_split(args: &[String]) -> Result<String, usize> {
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
            let tail = args[(i + 1).min(args.len())..]
                .iter()
                .map(|a| format!("'{}'", a.replace('\'', r"'\''")));
            return Ok(std::iter::once(s.to_string()).chain(tail).collect::<Vec<_>>().join(" "));
        }
        if a == "--" {
            return Err(i + 1);
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
    Err(i.min(args.len()))
}

/// A shell's options: (`-c` given, `-s` given, the first word after the options:
/// the `-c` script, else the script file). `bash -ce 'x'`, `bash -c -- 'x'` and
/// `bash -o pipefail -c 'x'` all give `x`.
fn shell_args(args: &[String]) -> (bool, bool, Option<&String>) {
    let (mut c, mut s) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return (c, s, it.next());
        }
        if a.starts_with("--") {
            if a == "--rcfile" || a == "--init-file" {
                it.next();
            }
            continue;
        }
        if a.len() > 1 && (a.starts_with('-') || a.starts_with('+')) {
            if a.starts_with('-') {
                c |= a.contains('c');
                s |= a.contains('s');
            }
            if a.ends_with('o') || a.ends_with('O') {
                it.next();
            }
            continue;
        }
        return (c, s, Some(a));
    }
    (c, s, None)
}

/// The value of `-<letter>` (alone, attached, or ending a cluster such as `-qc`)
/// or `--<long>` / `--<long>=v`, anywhere in `args`.
fn opt_value<'a>(args: &'a [String], letter: char, long: &str) -> Option<&'a str> {
    for (i, a) in args.iter().enumerate() {
        let next = || args.get(i + 1).map(String::as_str);
        if a == long {
            return next();
        }
        if let Some(v) = a.strip_prefix(long).and_then(|r| r.strip_prefix('=')) {
            return Some(v);
        }
        if a.len() > 1
            && a.starts_with('-')
            && !a.starts_with("--")
            && let Some(j) = a.find(letter)
        {
            let rest = &a[j + letter.len_utf8()..];
            return if rest.is_empty() { next() } else { Some(rest) };
        }
    }
    None
}

/// What a runner runs (L6.2, L6.8): `watch`, `flock`, `setsid`, `nsenter`,
/// `unshare`, `script -c`, `su -c`, `busybox`, `systemd-run`, `parallel`,
/// `find -exec`/`-execdir`/`-ok`, `git submodule foreach`, `trap '<script>'`.
fn runner<'a>(name: &str, args: &'a [String]) -> Vec<Inner<'a>> {
    let words = |w: &'a [String]| if w.is_empty() { vec![] } else { vec![Inner::Words(w)] };
    let script = |w: &[String]| {
        if w.is_empty() {
            vec![]
        } else {
            vec![Inner::Script(w.join(" "))]
        }
    };
    match name {
        "watch" => {
            let rest = skip_opts(args, &["-n", "--interval", "-q", "--equexit"]);
            if args.iter().any(|a| a == "-x" || a == "--exec") {
                words(rest)
            } else {
                // watch runs its words through `sh -c`.
                script(rest)
            }
        }
        "flock" => {
            // Options end at the lock file; `-c` is read only before it or right after
            // it, so `flock /tmp/l curl -c jar` runs `curl`.
            let rest = skip_opts(
                args,
                &["-w", "--timeout", "-E", "--conflict-exit-code", "-c", "--command"],
            );
            let opts = &args[..args.len() - rest.len()];
            match (opt_value(opts, 'c', "--command"), rest.get(1).map(String::as_str)) {
                (Some(s), _) => vec![Inner::Script(s.to_string())],
                (None, Some("-c" | "--command")) => script(rest.get(2..3).unwrap_or_default()),
                (None, _) => words(rest.get(1..).unwrap_or_default()),
            }
        }
        "setsid" => words(skip_opts(args, &[])),
        // `trap '<script>' SIGNAL…`: the shell runs the script later (on exit, a signal).
        "trap" => match skip_opts(args, &[]).first() {
            Some(body) if body != "-" => vec![Inner::Script(body.clone())],
            _ => vec![],
        },
        "nsenter" => words(skip_opts(args, &["-t", "--target", "-S", "--setuid", "-G", "--setgid"])),
        "unshare" => words(skip_opts(
            args,
            &[
                "--propagation",
                "--setgroups",
                "--map-user",
                "--map-group",
                "--map-users",
                "--map-groups",
                "-S",
                "--setuid",
                "-G",
                "--setgid",
                "-R",
                "--root",
                "-w",
                "--wd",
            ],
        )),
        "script" => opt_value(args, 'c', "--command")
            .map(|s| Inner::Script(s.to_string()))
            .into_iter()
            .collect(),
        "su" => opt_value(args, 'c', "--command")
            .or_else(|| opt_value(args, '\0', "--session-command"))
            .map(|s| Inner::Script(s.to_string()))
            .into_iter()
            .collect(),
        "busybox" => match args.first() {
            Some(a) if !a.starts_with('-') => words(args),
            _ => vec![],
        },
        "systemd-run" => words(skip_opts(
            args,
            &[
                "-u",
                "--unit",
                "-p",
                "--property",
                "--description",
                "--slice",
                "-E",
                "--setenv",
                "-M",
                "--machine",
                "-H",
                "--host",
                "--uid",
                "--gid",
                "--nice",
                "--working-directory",
                "--service-type",
                "--on-active",
                "--on-boot",
                "--on-startup",
                "--on-unit-active",
                "--on-unit-inactive",
                "--on-calendar",
                "--timer-property",
                "--path-property",
                "--socket-property",
            ],
        )),
        "parallel" => {
            let rest = skip_opts(
                args,
                &[
                    "-j",
                    "--jobs",
                    "-S",
                    "--sshlogin",
                    "--sshloginfile",
                    "-a",
                    "--arg-file",
                    "--colsep",
                    "-I",
                    "-d",
                    "--delimiter",
                    "-n",
                    "--max-args",
                    "-N",
                    "--results",
                    "--joblog",
                    "--tmpdir",
                    "--timeout",
                    "-E",
                    "--workdir",
                ],
            );
            let end = rest.iter().position(|a| a.starts_with(":::")).unwrap_or(rest.len());
            script(&rest[..end])
        }
        "find" => {
            let mut out = Vec::new();
            let mut i = 0;
            while i < args.len() {
                if ["-exec", "-execdir", "-ok", "-okdir"].contains(&args[i].as_str()) {
                    let end = args[i + 1..]
                        .iter()
                        .position(|a| a == ";" || a == "+")
                        .map_or(args.len(), |p| p + i + 1);
                    out.extend(words(&args[i + 1..end]));
                    i = end;
                }
                i += 1;
            }
            out
        }
        "git" => {
            let Some(i) = args.iter().position(|a| a == "submodule") else {
                return vec![];
            };
            let rest = &args[i + 1..];
            let j = rest.iter().position(|a| !a.starts_with('-')).unwrap_or(rest.len());
            if rest.get(j).is_some_and(|a| a == "foreach") {
                let cmd = &rest[j + 1..];
                let k = cmd.iter().position(|a| !a.starts_with('-')).unwrap_or(cmd.len());
                script(&cmd[k..])
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// An interpreter of the gate's list given code it can't read (L6.8): inline
/// (`-c`, `-e`, `deno eval`, …), or on stdin with no script file or module.
pub(crate) fn runs_inline(words: &[String], stdin: bool) -> bool {
    let Some(first) = words.first() else { return false };
    let name = cmd_name(first);
    let name = name.as_ref();
    if !is_interpreter(name) {
        return false;
    }
    let args = &words[1..];
    let (codes, seen, rest) = interpreter_args(name, args);
    !codes.is_empty() || (stdin && !seen.contains('m') && args.get(rest).is_none_or(|a| a == "-"))
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
    // Text fed on stdin (heredoc bodies, here-strings).
    let mut input: Vec<String> = Vec::new();
    let mut piped = false;
    let mut iter = toks.into_iter().peekable();
    loop {
        match iter.next() {
            Some(Tok::Word(w)) => words.push(w),
            Some(Tok::Heredoc(body)) => input.push(body),
            Some(Tok::HereString) => {
                if let Some(Tok::Word(w)) = iter.peek() {
                    input.push(w.clone());
                    iter.next();
                }
            }
            Some(Tok::Redirect) => {
                if let Some(Tok::Word(target)) = iter.next() {
                    risk = risk.max(redirect(&target, ctx));
                } else {
                    risk = risk.max(Risk::new(Level::Confirm, "redirect without a target"));
                }
            }
            sep @ (Some(Tok::Sep(_)) | None) => {
                if !words.is_empty() {
                    risk = risk.max(command(&words, &input, piped, ctx, depth));
                }
                words.clear();
                input.clear();
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
pub(crate) const SAFE: &[&str] = &[
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

/// `input` is the text fed on stdin by heredocs and here-strings.
fn command(words: &[String], input: &[String], piped: bool, ctx: &Ctx, depth: u32) -> Risk {
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
    let r = simple(w, input, piped, ctx, depth);
    // `GIT_EXTERNAL_DIFF=… git diff`, `PAGER=… git log`: the variable runs a program.
    if runs_var && r.level == Level::Safe {
        Risk::low()
    } else {
        r
    }
}

/// A simple command without its leading assignments: its own risk and that of
/// what a runner (`watch`, `setsid`, `find -exec`, `su -c`, …) runs.
fn simple(w: &[String], input: &[String], piped: bool, ctx: &Ctx, depth: u32) -> Risk {
    let Some(first) = w.first() else {
        return Risk::safe();
    };
    let name = cmd_name(first);
    let mut r = simple_base(w, input, piped, ctx, depth);
    for inner in runner(&name, &w[1..]) {
        let ran = match inner {
            Inner::Words(iw) => command(iw, input, piped, ctx, depth + 1),
            Inner::Script(s) => classify_depth(&s, ctx, depth + 1),
        };
        r = r.max(ran).max(Risk::low());
    }
    r
}

fn simple_base(w: &[String], input: &[String], piped: bool, ctx: &Ctx, depth: u32) -> Risk {
    let Some(first) = w.first() else {
        return Risk::safe();
    };
    if computed(w) {
        return Risk::new(Level::Confirm, "the command name is computed");
    }
    let name = cmd_name(first).into_owned();
    let args = &w[1..];
    let rest = || args.join(" ");
    match name.as_str() {
        "command" if lookup_only(args) => Risk::safe(),
        "sudo" | "doas" | "pkexec" => {
            let inner = skip_opts(args, SUDO_VALUE_FLAGS);
            let inner_risk = if inner.is_empty() {
                Risk::safe()
            } else {
                command(inner, input, piped, ctx, depth)
            };
            Risk::new(Level::Confirm, format!("runs as root ({name})")).max(inner_risk)
        }
        "env" => match env_split(args) {
            // `-S` splits this string into the command, and env appends the words after it.
            Ok(line) => classify_depth(&line, ctx, depth + 1),
            // command() skips (and judges) the `VAR=value` words.
            Err(i) if i < args.len() => command(&args[i..], input, piped, ctx, depth),
            Err(_) => Risk::safe(),
        },
        "timeout" => {
            // Options, then the duration, then the command.
            let rest = skip_opts(args, &["-s", "-k", "--signal", "--kill-after"]);
            match rest.get(1..) {
                Some(c) if !c.is_empty() => command(c, input, piped, ctx, depth),
                _ => Risk::low(),
            }
        }
        "xargs" => {
            let rest = skip_opts(args, XARGS_VALUE_FLAGS);
            if rest.is_empty() {
                Risk::low()
            } else {
                command(rest, input, piped, ctx, depth).max(Risk::low())
            }
        }
        n if WRAPPERS.contains(&n) => {
            let inner = skip_opts(args, wrapper_flags(n));
            if inner.is_empty() {
                Risk::low()
            } else {
                command(inner, input, piped, ctx, depth)
            }
        }
        n if SHELLS.contains(&n) => {
            let (c, s, first) = shell_args(args);
            if c {
                match first {
                    Some(script) => classify_depth(script, ctx, depth + 1).max(Risk::low()),
                    None => Risk::new(Level::Confirm, "shell -c without a script"),
                }
            } else if piped {
                Risk::new(Level::Confirm, "pipes downloaded or generated text into a shell")
            } else if s || first.is_none_or(|f| f == "-") {
                // A heredoc or here-string is the script.
                input
                    .iter()
                    .fold(Risk::low(), |r, body| r.max(classify_depth(body, ctx, depth + 1)))
            } else {
                Risk::low()
            }
        }
        "eval" => classify_depth(&rest(), ctx, depth + 1).max(Risk::low()),
        // Shell keywords: what follows them is the command (`$(…)` is checked on its own).
        "if" | "then" | "else" | "elif" | "while" | "until" | "do" | "!" | "coproc" => {
            if args.is_empty() {
                Risk::safe()
            } else {
                command(args, input, piped, ctx, depth)
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
                        && ["rm", "shred", "unlink"].contains(&cmd_name(&p[1]).as_ref())
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
        "sed" => sed(args, ctx),
        "tee" => tee(args, ctx),
        "awk" | "gawk" => awk(&name, args, ctx),
        n if is_interpreter(n) => interpreter(n, args, input, ctx),
        // Windows shells: novacode can't parse their commands, so they ask at least.
        #[cfg(windows)]
        "cmd" | "powershell" | "pwsh" => windows_shell(&name, args, ctx),
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

/// A wrapper's options that take a value.
fn wrapper_flags(n: &str) -> &'static [&'static str] {
    match n {
        "nice" => &["-n", "--adjustment"],
        "ionice" => &["-c", "-n", "-p", "-P", "-u"],
        "stdbuf" => &["-i", "-o", "-e"],
        "time" => &["-o", "-f", "--output", "--format"],
        "exec" => &["-a"],
        _ => &[],
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
    same(p, ctx.home) || is_system(p)
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
/// The files it edits in place are checked like a redirect target.
fn sed(args: &[String], ctx: &Ctx) -> Risk {
    let short = |a: &String, c: char| a.starts_with('-') && !a.starts_with("--") && a.contains(c);
    let in_place = args.iter().any(|a| a.starts_with("--in-place") || short(a, 'i'));
    let script_file = args.iter().any(|a| a.starts_with("--file") || short(a, 'f'));
    let writes = regex::Regex::new(r"(^|[/;{}0-9$p]\s*)[wWe](\s|;|$)").unwrap();
    if in_place {
        written(sed_files(args), ctx)
    } else if script_file || args.iter().any(|a| !a.starts_with('-') && writes.is_match(a)) {
        Risk::low()
    } else {
        Risk::safe()
    }
}

/// The files of a sed command: every plain word when a script is given with
/// `-e`/`-f`, else every plain word after the first (the script).
fn sed_files(args: &[String]) -> Vec<&str> {
    let mut pos = Vec::new();
    let mut script = false;
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        if a == "--" {
            pos.extend(it);
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or("");
            script |= name == "expression" || name == "file";
            if !long.contains('=') && ["expression", "file", "line-length"].contains(&name) {
                it.next();
            }
        } else if a.len() > 1 && a.starts_with('-') {
            for (j, c) in a.char_indices().skip(1) {
                match c {
                    'e' | 'f' | 'l' => {
                        script |= c != 'l';
                        if j + 1 == a.len() {
                            it.next();
                        }
                        break;
                    }
                    // The rest is the backup suffix (`-i.bak`).
                    'i' => break,
                    _ => {}
                }
            }
        } else {
            pos.push(a);
        }
    }
    if script { pos } else { pos.into_iter().skip(1).collect() }
}

/// A command that writes `files`: each is checked like a redirect target; at least Low.
fn written<S: AsRef<str>>(files: impl IntoIterator<Item = S>, ctx: &Ctx) -> Risk {
    files
        .into_iter()
        .fold(Risk::low(), |r, f| r.max(redirect(f.as_ref(), ctx)))
}

/// `tee`: every word that isn't an option is a file it writes.
fn tee(args: &[String], ctx: &Ctx) -> Risk {
    let files = match args.iter().position(|a| a == "--") {
        Some(i) => args[..i]
            .iter()
            .filter(|a| !a.starts_with('-') || *a == "-")
            .chain(&args[i + 1..])
            .collect::<Vec<_>>(),
        None => args.iter().filter(|a| !a.starts_with('-') || *a == "-").collect(),
    };
    written(files, ctx)
}

/// `awk`/`gawk`: `-i inplace` writes the files after the program, and the
/// program's `print > "path"` writes that path.
fn awk(name: &str, args: &[String], ctx: &Ctx) -> Risk {
    static TARGET: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"\bprintf?\b[^;}]*?>{1,2}\s*"([^"]+)""#).unwrap());
    let mut r = if name == "awk" && !writes_or_runs("awk", args) {
        Risk::safe()
    } else {
        Risk::low()
    };
    let o = getopt(
        args,
        "fvFilE",
        &["file", "assign", "field-separator", "include", "load", "exec"],
    );
    let (prog, files) = if o.has(|k| ["f", "file", "E", "exec"].contains(&k)) {
        (None, &o.pos[..])
    } else {
        match o.pos.split_first() {
            Some((p, f)) => (Some(*p), f),
            None => (None, &o.pos[..]),
        }
    };
    let inplace = o
        .opts
        .iter()
        .any(|(k, v)| (k == "i" || k == "include") && v.is_some_and(|v| v.trim_end_matches(".awk") == "inplace"));
    if inplace {
        r = r.max(written(files, ctx));
    }
    for t in prog.into_iter().flat_map(|p| TARGET.captures_iter(p)) {
        r = r.max(written([&t[1]], ctx));
    }
    r
}

/// Interpreters whose inline code (or a script fed on stdin) the gate can't read.
fn is_interpreter(name: &str) -> bool {
    name.strip_prefix("python")
        .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        || ["py", "node", "nodejs", "deno", "bun", "perl", "ruby", "php"].contains(&name)
}

/// A command's name as the shell finds it: its last path part. Windows also splits on
/// `\`, ignores case and drops `.exe`/`.cmd`/`.bat`/`.com` (`C:\Bin\RM.EXE` is `rm`).
pub(crate) fn cmd_name(w: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(unix)]
    return w.rsplit('/').next().unwrap_or(w).into();
    #[cfg(windows)]
    {
        let n = w.rsplit(['/', '\\']).next().unwrap_or(w).to_lowercase();
        let bare = [".exe", ".cmd", ".bat", ".com"].iter().find_map(|x| n.strip_suffix(x));
        bare.map(str::to_string).unwrap_or(n).into()
    }
}

/// `cmd`, `powershell`, `pwsh`: their commands aren't parsed. Shared when they name a
/// credential folder, home or a system folder; else Confirm.
#[cfg(windows)]
fn windows_shell(name: &str, args: &[String], ctx: &Ctx) -> Risk {
    let code = args.join(" ");
    let lower = code.to_lowercase().replace('\\', "/");
    let home = ctx.home.to_string_lossy().to_lowercase().replace('\\', "/");
    let names_home = [
        "userprofile",
        "homepath",
        "homedrive",
        "appdata",
        "systemroot",
        "windir",
        "~",
        home.as_str(),
    ]
    .iter()
    .any(|w| lower.contains(w));
    if let Some(what) = mentions(&code, ctx) {
        Risk::new(
            Level::Shared,
            format!("{name} code mentions {what}; novacode can't see what it does"),
        )
    } else if names_home {
        Risk::new(
            Level::Shared,
            format!("{name} code names home or a system folder; novacode can't see what it does"),
        )
    } else {
        Risk::new(
            Level::Confirm,
            format!("runs {name}, whose commands novacode can't read"),
        )
    }
}

/// Short options of an interpreter, up to the first plain word: the values of
/// the `code` letters (attached or the next word) and of the `long` names
/// (exact, the next word or `=value`). `value` letters take a value too;
/// `attached` letters take the rest of their word. Returns (code, letters
/// seen, index of the first word that isn't an option).
fn inline_code<'a>(
    args: &'a [String],
    code: &str,
    value: &str,
    attached: &str,
    long: &[&str],
) -> (Vec<&'a str>, String, usize) {
    let mut codes = Vec::new();
    let mut seen = String::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        i += 1;
        if a == "--" {
            break;
        }
        if long.contains(&a.as_str()) {
            codes.extend(args.get(i).map(String::as_str));
            i += 1;
            continue;
        }
        if let Some(v) = long.iter().find_map(|l| a.strip_prefix(&format!("{l}="))) {
            codes.push(v);
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        if a.len() < 2 || !a.starts_with('-') {
            i -= 1;
            break;
        }
        for (j, c) in a.char_indices().skip(1) {
            seen.push(c);
            let rest = &a[j + c.len_utf8()..];
            if code.contains(c) || value.contains(c) {
                let v = if rest.is_empty() {
                    i += 1;
                    args.get(i - 1).map(String::as_str)
                } else {
                    Some(rest)
                };
                if code.contains(c) {
                    codes.extend(v);
                }
                break;
            }
            if attached.contains(c) {
                break;
            }
        }
    }
    (codes, seen, i.min(args.len()))
}

/// [`inline_code`] with each interpreter's options.
fn interpreter_args<'a>(name: &str, args: &'a [String]) -> (Vec<&'a str>, String, usize) {
    match name {
        "node" | "nodejs" | "bun" => inline_code(args, "", "r", "", &["-e", "-p", "-pe", "-ep", "--eval", "--print"]),
        "deno" => match args.split_first() {
            Some((e, code)) if e == "eval" => (
                code.iter()
                    .map(String::as_str)
                    .filter(|a| !a.starts_with('-'))
                    .collect(),
                String::new(),
                args.len(),
            ),
            _ => (Vec::new(), String::new(), 0),
        },
        "perl" => inline_code(args, "eE", "", "iIMmlx0dCFDV", &[]),
        "ruby" => inline_code(args, "e", "IrCE", "F0x", &[]),
        "php" => inline_code(args, "r", "cdzfBRFE", "", &[]),
        _ => inline_code(args, "c", "WXm", "", &[]),
    }
}

/// An interpreter: inline code or stdin that names a credential folder or a
/// protected path is Shared (the user decides; a read can be legitimate).
/// `perl -i` writes the files after its code.
fn interpreter(name: &str, args: &[String], input: &[String], ctx: &Ctx) -> Risk {
    let (codes, seen, rest) = interpreter_args(name, args);
    let mut r = Risk::low();
    if let Some(what) = codes
        .iter()
        .copied()
        .chain(input.iter().map(String::as_str))
        .find_map(|t| mentions(t, ctx))
    {
        r = Risk::new(
            Level::Shared,
            format!("inline code mentions {what}; novacode can't see what it does"),
        );
    }
    if name == "perl" && seen.contains('i') {
        let files = &args[rest..];
        r = r.max(written(
            if codes.is_empty() {
                files.get(1..).unwrap_or_default()
            } else {
                files
            },
            ctx,
        ));
    }
    r
}

/// The credential folder or protected path that `code` names, if any:
/// `~/.ssh`, `$HOME/.ssh`, `${HOME}/.ssh` or the absolute path.
fn mentions(code: &str, ctx: &Ctx) -> Option<String> {
    // Windows: compared lower-cased with `/`, so `C:\Users\u\.SSH` is `c:/users/u/.ssh`.
    let norm = |s: &str| {
        if cfg!(windows) {
            // Collapsed: escaped strings in inline code (`'C:\\Users\\u\\.ssh'`) double the `\`.
            let mut s = s.replace('\\', "/").to_lowercase();
            while s.contains("//") {
                s = s.replace("//", "/");
            }
            s
        } else {
            s.to_string()
        }
    };
    let code = &norm(code);
    let home = norm(&ctx.home.to_string_lossy());
    let home_words: &[&str] = if cfg!(windows) {
        &["home", "userprofile"]
    } else {
        &["HOME"]
    };
    // (path under home, how to name it); protected paths outside home are matched whole.
    let mut under: Vec<(String, String)> = CREDENTIAL_DIRS
        .iter()
        .chain(WINDOWS_CREDENTIAL_DIRS)
        .map(|d| (norm(d), format!("~/{d}")))
        .collect();
    let mut outside = Vec::new();
    for extra in ctx.protected {
        let p = resolve(extra, ctx);
        let what = format!("the protected path {extra}");
        match p.strip_prefix(ctx.home) {
            Ok(rel) if !rel.as_os_str().is_empty() => under.push((norm(&rel.to_string_lossy()), what)),
            _ => outside.push((norm(&p.to_string_lossy()), what)),
        }
    }
    // A name ends where no path-name letter follows (`~/.aws` is not `~/.awsome`).
    let ends = |rest: &str| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '-');
    let found = |needle: &str| code.match_indices(needle).any(|(i, _)| ends(&code[i + needle.len()..]));
    for (rel, what) in &under {
        if found(&format!("~/{rel}")) || found(&format!("{home}/{rel}")) {
            return Some(what.clone());
        }
        // shortcut: `HOME`, then any punctuation, then `/<folder>` counts as a mention:
        // `$HOME/.ssh`, `${HOME}/.ssh`, `process.env.HOME+'/.aws'`, `os.environ['HOME'] + "/.ssh"`.
        // A path built any other way (a variable, `join`) is not seen; that is the sandbox's job.
        let tail = format!("/{rel}");
        let after_home = home_words.iter().any(|w| {
            code.match_indices(w).any(|(i, _)| {
                let rest = code[i + w.len()..].trim_start_matches(|c: char| !c.is_alphanumeric() && c != '/');
                rest.strip_prefix(&tail).is_some_and(ends)
            })
        });
        if after_home {
            return Some(what.clone());
        }
    }
    outside.into_iter().find(|(abs, _)| found(abs)).map(|(_, what)| what)
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
            // Attached form: `-ccore.fsmonitor=x`.
            a if a.starts_with("-c") && a.len() > 2 => {
                runs |= !harmless(&a[2..]);
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
pub(crate) fn resolve(arg: &str, ctx: &Ctx) -> PathBuf {
    let home = ctx.home.to_string_lossy();
    #[cfg(unix)]
    let mut s = arg.to_string();
    #[cfg(unix)]
    for var in ["${HOME}", "$HOME"] {
        if let Some(rest) = s.strip_prefix(var) {
            s = format!("{home}{rest}");
        }
    }
    #[cfg(windows)]
    let mut s = windows_vars(arg, &home);
    if s == "~" || s.starts_with("~/") {
        s = format!("{home}{}", &s[1..]);
    }
    #[cfg(windows)]
    let before = s.len();
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
    // `C:/` trimmed to `C:` would read as drive C's cwd.
    #[cfg(windows)]
    if s.len() == 2 && s.as_bytes()[1] == b':' && before > 2 {
        s.push('/');
    }
    if s == "*" || s == ".*" {
        s = ".".into();
    }
    // `\??\` is the NT form of `\\?\`.
    #[cfg(windows)]
    if let Some(r) = s.strip_prefix(r"\??\") {
        s = format!(r"\\?\{r}");
    }
    // Git Bash names drives `/c/...`.
    #[cfg(windows)]
    if let [b'/', d, rest @ ..] = s.as_bytes()
        && d.is_ascii_alphabetic()
        && (rest.is_empty() || rest[0] == b'/')
    {
        s = format!("{}:/{}", *d as char, s[2..].trim_start_matches('/'));
    }
    // `C:rest` is relative to drive C's cwd: ours when we're on C, else (unknown) its root.
    #[cfg(windows)]
    if s.as_bytes().get(1) == Some(&b':') && s.as_bytes()[0].is_ascii_alphabetic() && !s[2..].starts_with(['/', '\\']) {
        let on_cwd = fold(ctx.cwd)
            .to_string_lossy()
            .get(..2)
            .is_some_and(|c| c.eq_ignore_ascii_case(&s[..2]));
        s = if on_cwd {
            ctx.cwd.join(&s[2..]).to_string_lossy().into_owned()
        } else {
            format!("{}\\{}", &s[..2], &s[2..])
        };
    }
    let p = Path::new(&s);
    // A `/`-led path without a drive is Git Bash's own (`/`, `/dev/sda`); a `\`-led one
    // is on the cwd's drive, which `join` keeps. Unix: `/`-led is absolute.
    let joined = if p.is_absolute() || s.starts_with('/') {
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
    // 8.3 short names (`PROGRA~1`) and drive-less device paths (`\\?\Volume{…}`) only resolve on disk.
    #[cfg(windows)]
    if let o = out.to_string_lossy().into_owned()
        && (o.contains('~') || o.starts_with(r"\\") || o.starts_with("//"))
    {
        out = crate::policy::real(&out);
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

/// Windows' own credential stores under home, on top of [`CREDENTIAL_DIRS`].
#[cfg(windows)]
const WINDOWS_CREDENTIAL_DIRS: &[&str] = &[
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
    "AppData/Roaming/gcloud",
    "AppData/Roaming/GitHub CLI",
];
#[cfg(unix)]
const WINDOWS_CREDENTIAL_DIRS: &[&str] = &[];

/// The credential folders under home; the sandbox hides them too (L6.5).
pub fn credential_dirs() -> &'static [&'static str] {
    CREDENTIAL_DIRS
}

/// The form protected paths are compared in. Unix: as is.
#[cfg(unix)]
pub(crate) fn fold(p: &Path) -> std::borrow::Cow<'_, Path> {
    std::borrow::Cow::Borrowed(p)
}

/// Windows: every spelling of one file folds to one form, so
/// `\\.\C:\Users\X\.SSH.\k:s` and `\\localhost\c$\users\x\.ssh\k` are `C:\USERS\X\.SSH\K`:
/// the `\\?\`, `\\.\` and `\??\` prefixes and admin shares (`\\host\C$`) name a drive,
/// `name:stream` writes `name` (alternate data streams), and NTFS ignores case
/// (it compares upper-cased) and drops trailing dots and spaces.
#[cfg(windows)]
pub(crate) fn fold(p: &Path) -> std::borrow::Cow<'_, Path> {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = if let Some(r) = s.strip_prefix(r"\\?\UNC\").or_else(|| s.strip_prefix(r"\\.\UNC\")) {
        format!(r"\\{r}")
    } else if let Some(r) = [r"\\?\", r"\\.\", r"\??\"].iter().find_map(|x| s.strip_prefix(x)) {
        r.to_string()
    } else {
        s.clone()
    };
    let share = s
        .strip_prefix(r"\\")
        .and_then(|r| r.split_once('\\'))
        .and_then(|(_, r)| {
            let (share, rest) = r.split_once('\\').unwrap_or((r, ""));
            match share.as_bytes() {
                [d, b'$'] if d.is_ascii_alphabetic() => Some(format!("{}:\\{rest}", *d as char)),
                _ if share.eq_ignore_ascii_case("admin$") => {
                    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
                    Some(format!(r"{root}\{rest}"))
                }
                _ => None,
            }
        });
    let s = share.unwrap_or(s);
    // `C:rest` (drive-relative) is the drive, then `rest`; not a stream named `rest`.
    let s = match s.as_bytes() {
        [d, b':', x, ..] if d.is_ascii_alphabetic() && *x != b'\\' => format!(r"{}\{}", &s[..2], &s[2..]),
        _ => s,
    };
    // A drive inside the path (`C:\proj\C:\Users\u` after joining `\??\C:\…` or `C:\…` to a
    // cwd) is where the path really starts.
    let is_drive = |c: &str| matches!(c.as_bytes(), [d, b':'] if d.is_ascii_alphabetic());
    let s = match s.split('\\').skip(1).position(is_drive) {
        Some(i) => s.split('\\').skip(i + 1).collect::<Vec<_>>().join("\\"),
        None => s,
    };
    let parts: Vec<String> = s
        .split('\\')
        .enumerate()
        .map(|(i, c)| {
            let drive = i == 0 && matches!(c.as_bytes(), [d, b':'] if d.is_ascii_alphabetic());
            let c = if drive { c } else { c.split(':').next().unwrap_or("") };
            let c = if matches!(c, "" | "." | "..") {
                c
            } else {
                c.trim_end_matches(['.', ' '])
            };
            c.chars().flat_map(char::to_uppercase).collect()
        })
        .collect();
    std::borrow::Cow::Owned(PathBuf::from(parts.join("\\")))
}
/// `a` and `b` name the same place.
pub(crate) fn same(a: &Path, b: &Path) -> bool {
    fold(a) == fold(b)
}

/// `p` is `dir` or inside it.
pub(crate) fn under(p: &Path, dir: &Path) -> bool {
    fold(p).starts_with(fold(dir))
}

#[cfg(unix)]
fn is_system(p: &Path) -> bool {
    SYSTEM_DIRS.iter().any(|d| p == Path::new(d))
}

/// A drive root or one of Windows' own top folders on any drive.
#[cfg(windows)]
fn is_system(p: &Path) -> bool {
    let f = fold(p);
    let s = f.to_string_lossy();
    // No drive: Git Bash's `/`, `/usr`, … (its `/` holds the `/c` drive mounts).
    let Some(rest) = s
        .strip_prefix(|c: char| c.is_ascii_alphabetic())
        .and_then(|r| r.strip_prefix(':'))
    else {
        return SYSTEM_DIRS.iter().any(|d| f == fold(Path::new(d)));
    };
    [
        "",
        "WINDOWS",
        "PROGRAM FILES",
        "PROGRAM FILES (X86)",
        "PROGRAMDATA",
        "USERS",
    ]
    .contains(&rest.trim_matches('\\'))
}

/// Why writing a file at `path` would touch a protected place, or `None`: the
/// file tools refuse what a catastrophic shell command would (L6.2).
/// Under `~/.novacode` only the login and trust files count, so the agent can
/// still edit AGENTS.md, skills and settings there.
pub fn protected_path(path: &Path, ctx: &Ctx) -> Option<String> {
    let p = resolve(&path.to_string_lossy(), ctx);
    let folded = fold(&p);
    // Every form of home: `resolve` may have turned a short name into its long one.
    let homes = homes(ctx);
    let rest = homes.iter().find_map(|h| {
        let nova = fold(&h.join(".novacode")).into_owned();
        folded.strip_prefix(&nova).ok().map(Path::to_path_buf)
    });
    if let Some(rest) = rest
        && !rest.as_os_str().is_empty()
    {
        let mut parts = rest.components();
        let first = parts.next().map(|c| c.as_os_str().to_string_lossy().into_owned());
        // Names in `fold`'s form (upper case on Windows).
        let f = |s: &str| fold(Path::new(s)).to_string_lossy().into_owned();
        let cred = first.as_deref().is_some_and(|n| {
            n == f("auth.json")
                || n == f("mcp-auth.json")
                || n == f("trust.json")
                || (n.starts_with(&f("auth-")) && n.ends_with(&f(".json")))
        });
        if !(cred && parts.next().is_none()) {
            return extra_protected(&p, ctx);
        }
    }
    protected(&p, ctx)
}

/// Windows: every `$NAME`/`${NAME}` in `s`: the home folders from `home`, the rest from the
/// environment (`$SYSTEMDRIVE/`, `$HOMEDRIVE$HOMEPATH`). Unknown names stay as written.
#[cfg(windows)]
fn windows_vars(s: &str, home: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let r = &rest[i + 1..];
        let (name, after) = match r.strip_prefix('{').and_then(|b| b.split_once('}')) {
            Some(x) => x,
            None => {
                let end = r
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(r.len());
                (&r[..end], &r[end..])
            }
        };
        let value = match name.to_ascii_uppercase().as_str() {
            "" => None,
            "HOME" | "USERPROFILE" => Some(home.to_string()),
            "APPDATA" => Some(format!("{home}/AppData/Roaming")),
            "LOCALAPPDATA" => Some(format!("{home}/AppData/Local")),
            n => std::env::var(n).ok(),
        };
        match value {
            Some(v) => out.push_str(&v),
            None => out.push_str(&rest[i..rest.len() - after.len()]),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The home folders to protect: `ctx.home`, and on Windows `%USERPROFILE%` too,
/// where Windows keeps its own stores when `HOME` points elsewhere.
fn homes(ctx: &Ctx) -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut v = vec![ctx.home.to_path_buf()];
    // Also the long form of a short-named home (`RUNNER~1`): `resolve` expands short names.
    #[cfg(windows)]
    for h in [
        Some(crate::policy::real(ctx.home)),
        std::env::var_os("USERPROFILE").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    {
        if !v.iter().any(|x| same(x, &h)) {
            v.push(h);
        }
    }
    v
}

/// Why `p` is protected, or `None`.
fn protected(p: &Path, ctx: &Ctx) -> Option<String> {
    if is_system(p) {
        return Some(format!("the system folder {}", p.display()));
    }
    for home in homes(ctx) {
        if same(p, &home) {
            return Some("your home folder".into());
        }
        for d in CREDENTIAL_DIRS.iter().chain(WINDOWS_CREDENTIAL_DIRS) {
            if under(p, &home.join(d)) {
                return Some(format!("credentials in ~/{d}"));
            }
        }
    }
    extra_protected(p, ctx)
}

/// The user's own protected paths from settings.
fn extra_protected(p: &Path, ctx: &Ctx) -> Option<String> {
    for extra in ctx.protected {
        let e = resolve(extra, ctx);
        if under(p, &e) {
            return Some(format!("the protected path {extra}"));
        }
    }
    None
}

fn is_device(p: &Path) -> bool {
    let s = p.to_string_lossy();
    // Windows keeps `/dev/sda` as `\dev\sda`; Git Bash maps it to the raw disk.
    let s = if cfg!(windows) { s.replace('\\', "/").into() } else { s };
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

    /// L11.14: case, trailing dots and spaces, `\\?\`, `/c/` drive paths and
    /// `$USERPROFILE` all reach the same protected place.
    #[cfg(windows)]
    #[test]
    fn windows_paths_fold_before_matching() {
        let protected = vec!["~/secrets".to_string()];
        let ctx = Ctx {
            cwd: Path::new(r"C:\Users\u\proj"),
            home: Path::new(r"C:\Users\u"),
            protected: &protected,
        };
        let hit = |p: &str| protected_path(Path::new(p), &ctx);
        for p in [
            r"C:\Users\u\.ssh\authorized_keys",
            r"c:/users/U/.SSH/authorized_keys",
            r"C:\Users\u\.ssh.\authorized_keys",
            r"C:\Users\u\.ssh \k",
            r"\\?\C:\Users\u\.ssh\k",
            r"C:\Users\u\AppData\Roaming\Microsoft\Protect\x",
            r"C:\Users\u\SECRETS\a",
            r"C:\Users\u\.novacode\AUTH.json",
            // Review a1: streams, device and admin-share prefixes.
            r"C:\Users\u\.novacode\trust.json::$DATA",
            r"C:\Users\u\.ssh::$INDEX_ALLOCATION\authorized_keys",
            r"\\.\C:\Users\u\.ssh\authorized_keys",
            r"\??\C:\Users\u\.ssh\k",
            r"\\localhost\C$\Users\u\.ssh\k",
            r"\\127.0.0.1\c$\users\u\.ssh\k",
            r"\\?\UNC\localhost\C$\Users\u\.ssh\k",
            // Review a2: drive-relative, ADMIN$.
            r"C:..\.ssh\k",
            r"C:..\.ssh",
            r"\\localhost\ADMIN$",
            r"C:\Users\u",
            r"C:\",
            r"C:\Windows",
            r"d:\Program Files",
        ] {
            assert!(hit(p).is_some(), "{p}");
        }
        for p in [
            r"C:\Users\u\proj\a.txt",
            r"C:\Users\u\.sshx\k",
            r"C:\Users\u\.novacode\AGENTS.md",
        ] {
            assert!(hit(p).is_none(), "{p}: {:?}", hit(p));
        }
        let lvl = |c: &str| classify(c, &ctx).level;
        for c in [
            "rm -rf /c/Users/u/.ssh",
            "rm -rf $USERPROFILE/.ssh",
            "rm -rf ~/.SSH",
            "echo x > /c/users/u/.ssh/authorized_keys",
            "rm -rf //localhost/c$/Users/u/.ssh",
            r"rm -rf '\Users\u\.ssh'",
            r"rm -rf '\Windows'",
            "rm -rf $APPDATA/Microsoft/Protect",
            "rm -rf ${LOCALAPPDATA}/Microsoft/Credentials",
            // Review a2 F1: Windows spellings of the command name.
            "rm.exe -rf ~",
            "RM -rf ~",
            "/usr/bin/rm.exe -rf ~",
            r"'C:\Git\usr\bin\rm.exe' -rf ~",
            // F2: drive-relative paths.
            "rm -rf C:../.ssh",
            r"rm -rf 'C:..\.ssh'",
            "rm -rf C:/",
            // F3: Windows variables.
            "rm -rf $SYSTEMDRIVE/",
            "rm -rf $WINDIR",
            // F4: admin share.
            r"rm -rf '\\localhost\ADMIN$'",
        ] {
            assert_eq!(lvl(c), Catastrophic, "{c}");
        }
        assert_ne!(lvl("rm -rf /c/Users/u/proj/target"), Catastrophic);
        // Inline code naming a credential folder in Windows form asks, as on Unix.
        assert_eq!(lvl(r#"python3 -c "open(r'C:\Users\u\.SSH\id_rsa','a')""#), Shared);
        for c in [
            // F1: interpreters and Windows shells by any name; F5: escaped strings, USERPROFILE.
            r#"python.exe -c 'open("C:\\Users\\u\\.ssh\\id_rsa")'"#,
            r#"py -c "open(r'C:\Users\u\.ssh\k')""#,
            r#"node.exe -e "require('fs').rmSync(process.env.USERPROFILE+'/.ssh')""#,
            r#"cmd //c "rd /s /q %USERPROFILE%""#,
            r#"powershell -c "Remove-Item -Recurse ~\.ssh""#,
        ] {
            assert_eq!(lvl(c), Shared, "{c}");
        }
        assert_eq!(lvl("cmd /c dir"), Confirm);
        if std::env::var_os("HOMEDRIVE").is_some() && std::env::var_os("HOMEPATH").is_some() {
            assert_eq!(lvl(r#"rm -rf "$HOMEDRIVE$HOMEPATH""#), Catastrophic);
        }
        // A home given by its short name (CI's `%TEMP%` is `RUNNER~1`) is still home:
        // `rm -rf ~` once got through and ran on the runner.
        let short = std::env::temp_dir();
        let sctx = Ctx {
            cwd: &short,
            home: &short,
            protected: &protected,
        };
        for c in ["rm -rf ~", "rm -rf ~/.ssh", "rm -rf $HOME"] {
            assert_eq!(
                classify(c, &sctx).level,
                Catastrophic,
                "{c} with home {}",
                short.display()
            );
        }
        // 8.3 short names resolve on disk (when the drive has them).
        if Path::new(r"C:\PROGRA~1").exists() {
            assert_eq!(lvl("rm -rf C:/PROGRA~1"), Catastrophic);
        }
    }

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
            // Keywords, runners and shell scripts on stdin don't hide the command (L6.2/L6.8).
            ("if true; then rm -rf ~; fi", Catastrophic),
            ("for d in x; do rm -rf ~; done", Catastrophic),
            ("! rm -rf ~", Catastrophic),
            ("find . -exec rm -rf ~ \\;", Catastrophic),
            ("find . -execdir rm -rf ~ +", Catastrophic),
            ("sh <<'EOF'\nrm -rf ~\nEOF", Catastrophic),
            ("bash <<< 'rm -rf ~'", Catastrophic),
            ("bash -s <<EOF\nrm -rf ~\nEOF", Catastrophic),
            ("bash script.sh <<EOF\nrm -rf ~\nEOF", Low),
            ("bash <<'EOF'\necho hi\nEOF", Low),
            ("bash -ce 'rm -rf ~'", Catastrophic),
            ("bash -c -- 'rm -rf ~'", Catastrophic),
            ("bash -o pipefail -c 'rm -rf ~'", Catastrophic),
            ("setsid rm -rf ~", Catastrophic),
            ("watch 'rm -rf ~'", Catastrophic),
            ("watch -n 1 rm -rf ~", Catastrophic),
            ("flock /tmp/l rm -rf ~", Catastrophic),
            ("flock /tmp/l -c 'rm -rf ~'", Catastrophic),
            ("flock /tmp/l rm -c x -rf ~", Catastrophic),
            ("flock -w 5 /tmp/l rm -rf ~", Catastrophic),
            ("coproc rm -rf ~", Catastrophic),
            ("nsenter -t 1 rm -rf ~", Catastrophic),
            ("unshare -r rm -rf ~", Catastrophic),
            ("script -qc 'rm -rf ~' /dev/null", Catastrophic),
            ("su -c 'rm -rf ~'", Catastrophic),
            ("busybox rm -rf ~", Catastrophic),
            ("systemd-run --user rm -rf ~", Catastrophic),
            ("parallel 'rm -rf ~' ::: a", Catastrophic),
            ("git submodule foreach 'rm -rf ~'", Catastrophic),
            ("setsid cargo build", Low),
            // A `trap` body is a script the shell runs later (R15/L6.2).
            ("trap 'rm -rf ~' EXIT", Catastrophic),
            ("trap -- 'rm -rf ~' EXIT INT", Catastrophic),
            ("trap \"sh -c 'rm -rf /'\" EXIT", Catastrophic),
            ("trap 'echo bye' EXIT", Low),
            ("trap - EXIT", Low),
            // A computed command name can't be read (R15/L6.2).
            ("x=rm; $x -rf ~", Confirm),
            ("${CMD} build", Confirm),
            ("$(echo rm) -rf ~", Confirm),
            ("`echo rm` -rf ~", Confirm),
            ("sudo $x", Confirm),
            // `command -v`/`-V` only looks a name up.
            ("command -v curl", Safe),
            ("command -V rm -rf", Safe),
            ("command -pv git", Safe),
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
            ("git -ccore.fsmonitor='touch x' status", Low),
            ("git -ccolor.ui=always status", Safe),
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
            // Explicit writers are checked like a redirect (E1/L6.2).
            ("tee -a ~/.ssh/authorized_keys", Catastrophic),
            ("echo x | tee ~/.novacode/settings.json", Catastrophic),
            ("echo x | tee -a out.txt -- ~/.aws/config", Catastrophic),
            ("tee out.txt", Low),
            ("echo x | tee /dev/null", Low),
            ("sed -i s/a/b/ ~/.ssh/config", Catastrophic),
            ("sed -i s/a/b/ ~/.novacode/settings.json", Catastrophic),
            ("sed --in-place=.bak -e s/a/b/ ~/secrets/x", Catastrophic),
            ("sed -i.bak s/a/b/ notes.txt ~/.kube/config", Catastrophic),
            ("sed -i -e s/a/b/ notes.txt", Low),
            ("sed -i ~/.ssh/x notes.txt", Low),
            ("sed -n p ~/.ssh/config", Safe),
            ("perl -pi -e 's/a/b/' ~/.gnupg/gpg.conf", Catastrophic),
            ("perl -i.bak -pe 's/a/b/' notes.txt", Low),
            ("perl -pe 's/a/b/' ~/.gnupg/gpg.conf", Low),
            ("awk '{print > \"/home/u/.ssh/x\"}' f", Catastrophic),
            ("awk '{printf \"%s\", $1 >> \"/home/u/.aws/c\"}' f", Catastrophic),
            ("gawk -i inplace '{print}' ~/.ssh/config", Catastrophic),
            ("awk -i inplace '{print}' notes.txt", Low),
            ("awk '{print}' ~/.ssh/config", Safe),
            // Inline code naming a credential folder asks (E1/L6.2).
            (
                "python3 -c \"open('/home/u/.ssh/authorized_keys','a').write('k')\"",
                Shared,
            ),
            (
                "node -e \"require('fs').readFileSync(process.env.HOME+'/.aws/credentials')\"",
                Shared,
            ),
            ("node --eval=\"x(process.env.HOME + \\\"/.kube\\\")\"", Shared),
            ("python3 - <<EOF\nopen('~/.ssh/x')\nEOF", Shared),
            ("node <<'EOF'\nfs.readFileSync(`${HOME}/.gnupg/k`)\nEOF", Shared),
            ("python3 <<< \"open('$HOME/.ssh/x')\"", Shared),
            (
                "python3.12 -c \"import os; os.path.expanduser('~/.novacode/auth.json')\"",
                Shared,
            ),
            ("sudo python -Bc 'open(\"/home/u/.password-store/x\")'", Shared),
            ("deno eval 'Deno.readTextFile(\"/home/u/.ssh/id\")'", Shared),
            ("bun -e 'Bun.file(\"~/.aws/x\")'", Shared),
            ("perl -e 'open F, \"<$ENV{HOME}/.ssh/x\"'", Shared),
            ("ruby -e 'File.read(\"~/.config/gcloud/x\")'", Shared),
            ("php -r 'file_get_contents(\"~/secrets/x\");'", Shared),
            ("python3 -c 'print(\"~/.awsome\")'", Low),
            ("python3 -c 'print(1)'", Low),
            ("python3 script.py ~/.ssh/x", Low),
            (
                "python3 -c 'import shutil; shutil.rmtree(\"/\")' && rm -rf ~",
                Catastrophic,
            ),
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
