//! Secret masking (L6.6): novacode's own keys and tokens are replaced by the
//! name of their source before tool output reaches the model. With
//! `security.maskSecrets`, well-known token shapes (GitHub, AWS, OpenAI,
//! Anthropic, Slack, Google, private keys, `*_SECRET=`-style assignments) are
//! masked too, wherever they come from.

use regex::Regex;
use std::sync::{Arc, LazyLock};

/// (secret value, source label) pairs.
pub type Secrets = Vec<(String, String)>;

/// Reads the current secrets on every use, so a token refreshed mid-session is still masked.
#[derive(Clone)]
pub struct Redactor(Arc<dyn Fn() -> Secrets + Send + Sync>, bool);

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Redactor")
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new(Vec::new)
    }
}

/// Shorter values are skipped: masking them would hit ordinary text.
const MIN_LEN: usize = 8;

impl Redactor {
    pub fn new(source: impl Fn() -> Secrets + Send + Sync + 'static) -> Self {
        Self(Arc::new(source), false)
    }

    /// Also mask well-known secret shapes (`security.maskSecrets`).
    pub fn with_patterns(mut self, on: bool) -> Self {
        self.1 = on;
        self
    }

    /// `text` with every known secret replaced by `[redacted: <source>]`; `None` if nothing matched.
    pub fn apply(&self, text: &str) -> Option<String> {
        let mut out: Option<String> = None;
        for (secret, source) in (self.0)() {
            if secret.len() < MIN_LEN {
                continue;
            }
            let cur = out.as_deref().unwrap_or(text);
            if cur.contains(secret.as_str()) {
                out = Some(cur.replace(secret.as_str(), &format!("[redacted: {source}]")));
            }
        }
        if self.1 {
            for (re, label) in PATTERNS.iter() {
                let cur = out.as_deref().unwrap_or(text);
                if re.is_match(cur) {
                    // Group 1, when present, is a prefix to keep (`NAME=` of an assignment).
                    let r = re.replace_all(cur, |c: &regex::Captures| {
                        format!("{}[redacted: {label}]", c.get(1).map_or("", |m| m.as_str()))
                    });
                    out = Some(r.into_owned());
                }
            }
        }
        out
    }
}

static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----", "private key"),
        (r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}\b|\bgithub_pat_[A-Za-z0-9_]{50,}", "GitHub token"),
        (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "AWS access key"),
        (r"\bsk-ant-[A-Za-z0-9_-]{20,}", "Anthropic key"),
        (r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}", "OpenAI key"),
        (r"\bxox[abprs]-[A-Za-z0-9-]{10,}", "Slack token"),
        (r"\bAIza[0-9A-Za-z_-]{35}\b", "Google API key"),
        // NAME=value where NAME (any case) says it's a secret: keep the name, mask the value.
        // Parentheses end a value, so code such as `token = parse();` is left alone.
        (
            r#"(?mi)(\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|PRIVATE_KEY|ACCESS_KEY)[A-Z0-9_]*\s*[=:]\s*["']?)[^\s"'()]{8,}"#,
            "secret value",
        ),
    ]
    .into_iter()
    .map(|(p, l)| (Regex::new(p).expect("secret pattern"), l))
    .collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_known_secrets_only() {
        let r = Redactor::new(|| {
            vec![
                ("sk-ant-abcdef123".into(), "ANTHROPIC_API_KEY".into()),
                ("short".into(), "x".into()),
            ]
        });
        assert_eq!(
            r.apply("key=sk-ant-abcdef123 and short").as_deref(),
            Some("key=[redacted: ANTHROPIC_API_KEY] and short")
        );
        assert_eq!(r.apply("nothing here"), None);
        assert_eq!(
            r.apply("GITHUB_TOKEN=ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"),
            None,
            "patterns are opt-in"
        );
    }

    #[test]
    fn masks_secret_shapes_when_on() {
        let r = Redactor::default().with_patterns(true);
        let env = "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY\nGITHUB_TOKEN=ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8\nDEBUG=true\nid AKIAIOSFODNN7EXAMPLE";
        let out = r.apply(env).unwrap();
        assert!(
            !out.contains("wJalr") && !out.contains("ghp_A1") && !out.contains("AKIAIOSF"),
            "{out}"
        );
        assert!(
            out.contains("AWS_SECRET_ACCESS_KEY=[redacted") && out.contains("DEBUG=true"),
            "{out}"
        );
        assert_eq!(
            r.apply("let token = parse(); // tokens: 3"),
            None,
            "code isn't a secret"
        );
        assert_eq!(r.apply("max_tokens: 4096"), None);
    }

    #[test]
    fn secret_names_match_in_any_case() {
        let r = Redactor::default().with_patterns(true);
        assert_eq!(
            r.apply("api_key=abcdefgh1234 Api_Token: 'zyxwvuts9876'").as_deref(),
            Some("api_key=[redacted: secret value] Api_Token: '[redacted: secret value]'")
        );
        assert_eq!(r.apply("password=short"), None, "8-character minimum");
    }
}
