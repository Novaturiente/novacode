//! Secret masking (L6.6): novacode's own keys and tokens are replaced by the
//! name of their source before tool output reaches the model.

use std::sync::Arc;

/// (secret value, source label) pairs.
pub type Secrets = Vec<(String, String)>;

/// Reads the current secrets on every use, so a token refreshed mid-session is still masked.
#[derive(Clone)]
pub struct Redactor(Arc<dyn Fn() -> Secrets + Send + Sync>);

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
        Self(Arc::new(source))
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
        out
    }
}

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
    }
}
