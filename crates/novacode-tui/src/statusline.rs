//! The configurable status line (L9.12): rows of left/right segments from the
//! `ui.statusline` setting. Segment values come from the app each draw; extensions
//! add `ext:<key>` segments (L9 UI API).
//!
//! ```json
//! "ui": { "statusline": { "placement": "below", "rows": [
//!   { "left": ["activity"], "right": ["ext", "bg", {"id": "ctx", "thresholds": [50, 70, 90]}, "cache", "model", "cwd", "git"] }
//! ] } }
//! ```

use crate::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub const SEGMENTS: &[&str] = &[
    "activity",
    "model",
    "thinking",
    "cwd",
    "git",
    "ctx",
    "cache",
    "usage_5h",
    "usage_week",
    "cost",
    "turn_time",
    "session_time",
    "bg",
    "agents",
    "queue",
    "mcp",
    "wtpath",
    "compactions",
];

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Seg {
    pub id: String,
    /// `{}` is the value: `"ctx {}"`.
    #[serde(default)]
    pub format: Option<String>,
    /// A theme token or `#rrggbb`; overrides the segment's own colour.
    #[serde(default)]
    pub style: Option<String>,
    /// For percentage segments: ok below the 1st, warn below the 2nd, code below the 3rd, else error.
    #[serde(default)]
    pub thresholds: Option<[f64; 3]>,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
enum SegSpec {
    Id(String),
    Full(Seg),
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Row {
    #[serde(default, deserialize_with = "segs")]
    pub left: Vec<Seg>,
    #[serde(default, deserialize_with = "segs")]
    pub right: Vec<Seg>,
    /// Between segments (default two spaces).
    #[serde(default = "two")]
    pub sep: String,
}

fn two() -> String {
    "  ".into()
}

fn segs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Seg>, D::Error> {
    let v: Vec<SegSpec> = Vec::deserialize(d)?;
    Ok(v.into_iter()
        .map(|s| match s {
            SegSpec::Id(id) => Seg {
                id,
                format: None,
                style: None,
                thresholds: None,
                icon: None,
            },
            SegSpec::Full(s) => s,
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub rows: Vec<Row>,
    /// Draw the rows above the editor instead of below.
    pub above: bool,
}

impl Default for Config {
    fn default() -> Self {
        let ids = |v: &[&str]| {
            v.iter()
                .map(|id| Seg {
                    id: id.to_string(),
                    format: None,
                    style: None,
                    thresholds: None,
                    icon: None,
                })
                .collect()
        };
        Config {
            rows: vec![Row {
                left: ids(&["activity"]),
                // Dropped from the end on narrow screens: extension segments stay longest.
                right: ids(&[
                    "ext",
                    "bg",
                    "compactions",
                    "ctx",
                    "cache",
                    "cost",
                    "model",
                    "cwd",
                    "git",
                ]),
                sep: two(),
            }],
            above: false,
        }
    }
}

impl Config {
    /// `ui.statusline`; bad parts fall back to the default with a warning.
    pub fn parse(v: Option<&Value>) -> (Config, Option<String>) {
        let Some(v) = v else { return (Config::default(), None) };
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            rows: Option<Vec<Row>>,
            #[serde(default)]
            placement: Option<String>,
        }
        match serde_json::from_value::<File>(v.clone()) {
            Ok(f) => {
                let mut c = Config::default();
                if let Some(r) = f.rows {
                    c.rows = r;
                }
                c.above = f.placement.as_deref() == Some("above");
                let unknown: Vec<String> = c
                    .rows
                    .iter()
                    .flat_map(|r| r.left.iter().chain(&r.right))
                    .map(|s| s.id.clone())
                    .filter(|id| !SEGMENTS.contains(&id.as_str()) && id != "ext" && !id.starts_with("ext:"))
                    .collect();
                let warn = (!unknown.is_empty())
                    .then(|| format!("settings ui.statusline: unknown segments {}", unknown.join(", ")));
                (c, warn)
            }
            Err(e) => (
                Config::default(),
                Some(format!("settings ui.statusline: {e}; using the default")),
            ),
        }
    }
}

/// One segment's current value: text, its own style, and a number for thresholds.
pub struct Value_ {
    pub spans: Vec<Span<'static>>,
    pub pct: Option<f64>,
}

impl Value_ {
    pub fn text(s: impl Into<String>, st: Style) -> Self {
        Value_ {
            spans: vec![Span::styled(s.into(), st)],
            pct: None,
        }
    }
}

/// The `compactions` segment (L9.19): `compacted ×N`, nothing before the first one.
pub fn compactions(n: u32, t: &Theme) -> Option<Value_> {
    (n > 0).then(|| Value_::text(format!("compacted ×{n}"), t.dim))
}

/// Lay out every row. `values` holds the segments that have something to show;
/// `ext` lists the extension segments for the `ext` placeholder (all of them).
pub fn render(
    c: &Config,
    values: &BTreeMap<String, Value_>,
    ext: &[String],
    width: usize,
    t: &Theme,
) -> Vec<Line<'static>> {
    c.rows
        .iter()
        .map(|row| {
            let side = |segs: &[Seg]| -> Vec<Vec<Span<'static>>> {
                let mut out = Vec::new();
                for s in segs {
                    let ids: Vec<String> = if s.id == "ext" {
                        ext.to_vec()
                    } else {
                        vec![s.id.clone()]
                    };
                    for id in ids {
                        let Some(v) = values.get(&id) else { continue };
                        let mut spans = v.spans.clone();
                        if let (Some(th), Some(p)) = (s.thresholds, v.pct) {
                            let st = if p < th[0] {
                                t.ok
                            } else if p < th[1] {
                                t.warn
                            } else if p < th[2] {
                                t.code
                            } else {
                                t.error
                            };
                            spans.iter_mut().for_each(|sp| sp.style = st);
                        }
                        if let Some(st) = &s.style {
                            let st = t.token(Some(st));
                            spans.iter_mut().for_each(|sp| sp.style = st);
                        }
                        if let Some(f) = &s.format {
                            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
                            let st = spans.first().map(|s| s.style).unwrap_or_default();
                            spans = vec![Span::styled(f.replace("{}", &text), st)];
                        }
                        if let Some(i) = &s.icon {
                            spans.insert(
                                0,
                                Span::styled(format!("{i} "), spans.first().map(|s| s.style).unwrap_or_default()),
                            );
                        }
                        out.push(spans);
                    }
                }
                out
            };
            let join = |parts: Vec<Vec<Span<'static>>>| -> Vec<Span<'static>> {
                let mut v = Vec::new();
                for (i, p) in parts.into_iter().enumerate() {
                    if i > 0 {
                        v.push(Span::styled(row.sep.clone(), t.dim));
                    }
                    v.extend(p);
                }
                v
            };
            let left = join(side(&row.left));
            let mut right_parts = side(&row.right);
            // Narrow screen: keep the activity; drop right-side segments from the end.
            while !right_parts.is_empty() && crate::wrap::width(&join(right_parts.clone())) + 12 > width {
                right_parts.pop();
            }
            let mut right = join(right_parts);
            right.push(Span::raw(" "));
            let rw = crate::wrap::width(&right);
            let l = crate::wrap::truncate(left, width.saturating_sub(rw + 1));
            let pad = width.saturating_sub(crate::wrap::width(&l.spans) + rw);
            let mut spans = l.spans;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.extend(right);
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_segments_and_thresholds() {
        let t = Theme::default();
        let (c, warn) = Config::parse(Some(&json!({"placement": "above", "rows": [
            {"left": ["activity"], "right": [{"id": "ctx", "format": "ctx {}", "thresholds": [50, 70, 90]}, "model", "ext"]},
            {"right": ["nope"]}
        ]})));
        assert!(c.above);
        assert_eq!(c.rows.len(), 2);
        assert!(warn.unwrap().contains("nope"));
        let mut v = BTreeMap::new();
        v.insert("activity".into(), Value_::text(" ready", t.dim));
        v.insert(
            "ctx".into(),
            Value_ {
                spans: vec![Span::raw("95%")],
                pct: Some(95.0),
            },
        );
        v.insert("model".into(), Value_::text("claude/opus", t.accent));
        v.insert("ext:deploy".into(), Value_::text("deploying", t.text));
        let lines = render(&c, &v, &["ext:deploy".into()], 80, &t);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with(" ready") && text.trim_end().ends_with("ctx 95%  claude/opus  deploying"),
            "{text}"
        );
        let ctx = lines[0].spans.iter().find(|s| s.content == "ctx 95%").unwrap();
        assert_eq!(ctx.style, t.error, "above the last threshold");
        assert_eq!(Config::parse(Some(&json!({"rows": 3}))).0, Config::default());
    }

    #[test]
    fn compactions_segment_counts_and_hides_at_zero() {
        let t = Theme::default();
        assert!(compactions(0, &t).is_none());
        let v = compactions(2, &t).unwrap();
        let text: String = v.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "compacted ×2");
        assert_eq!(v.spans[0].style, t.dim);
        let c = Config::default();
        let ids: Vec<&str> = c.rows[0].right.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids[2..4], ["compactions", "ctx"]);
    }
}
