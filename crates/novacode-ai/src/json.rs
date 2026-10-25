//! Tool-argument parsing with repair (L2.4).

use serde_json::{Map, Value};

/// Parse tool arguments. Returns `({}, Some(raw))` when the text isn't a JSON
/// object even after repair, so the model sees its own text and can fix it.
pub fn parse_tool_args(raw: &str) -> (Value, Option<String>) {
    if raw.trim().is_empty() {
        return (Value::Object(Map::new()), None);
    }
    for candidate in [raw.to_string(), repair(raw)] {
        if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&candidate) {
            return (v, None);
        }
    }
    (Value::Object(Map::new()), Some(raw.to_string()))
}

/// Escape raw control characters inside strings and double backslashes that
/// don't start a valid escape (common model mistakes, e.g. Windows paths).
fn repair(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    let mut in_str = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if !in_str {
            in_str = c == '"';
            out.push(c);
            continue;
        }
        match c {
            '"' => {
                in_str = false;
                out.push(c);
            }
            // `\u` is an escape only with 4 hex digits (`\usepackage` isn't).
            '\\' if chars.peek() == Some(&'u')
                && chars.clone().skip(1).take(4).filter(char::is_ascii_hexdigit).count() == 4 =>
            {
                out.push(c);
                out.push(chars.next().unwrap_or_default());
            }
            '\\' => match chars.peek() {
                Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => {
                    out.push(c);
                    out.push(chars.next().unwrap_or_default());
                }
                _ => out.push_str("\\\\"),
            },
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repairs_and_reports() {
        assert_eq!(parse_tool_args(r#"{"a":1}"#), (json!({"a":1}), None));
        assert_eq!(parse_tool_args(""), (json!({}), None));
        assert_eq!(
            parse_tool_args("{\"p\":\"C:\\dir\\x\",\"t\":\"a\nb\"}").0,
            json!({"p":"C:\\dir\\x","t":"a\nb"})
        );
        assert_eq!(
            parse_tool_args(r#"{"t":"\usepackage \u00e9"}"#),
            (json!({"t":"\\usepackage é"}), None)
        );
        assert_eq!(parse_tool_args("[1]"), (json!({}), Some("[1]".into())));
        assert_eq!(parse_tool_args(r#"{"a":"#).1.as_deref(), Some(r#"{"a":"#));
    }
}
