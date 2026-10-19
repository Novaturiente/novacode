//! `schedule` (L10.5): run a message later or on an interval, inside this
//! session. Schedules live in the tool's result details (like `todo`), so the
//! session file is the store; each firing leaves a saved reminder that marks it
//! done. Due items fire when the session is resumed. No system cron.

use crate::hub::HubSlot;
use futures_util::future::BoxFuture;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub message: String,
    /// Unix ms of the next firing.
    pub next_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<u64>,
}

pub struct Schedule {
    items: Mutex<Vec<Item>>,
    next_id: Mutex<u32>,
    wake: Arc<Notify>,
}

/// `90s`, `10m`, `2h`, `1d`, `1h30m` → ms.
pub fn parse_duration(s: &str) -> Option<u64> {
    let (mut total, mut num) = (0u64, String::new());
    for c in s.trim().chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u64 = num.parse().ok()?;
            num.clear();
            total += n * match c {
                's' => 1_000,
                'm' => 60_000,
                'h' => 3_600_000,
                'd' => 86_400_000,
                _ => return None,
            };
        }
    }
    (num.is_empty() && total > 0).then_some(total)
}

/// `HH:MM` local → the next such moment, in Unix ms.
// shortcut: asks GNU `date` for the local zone (no tz crate); upgrade if a non-Linux build needs it.
fn parse_at(s: &str, now: u64) -> Option<u64> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 23 || m > 59 {
        return None;
    }
    let out = std::process::Command::new("date")
        .args(["-d", &format!("{h:02}:{m:02}"), "+%s"])
        .output()
        .ok()?;
    let mut t: u64 = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()? * 1000;
    while t <= now {
        t += 86_400_000;
    }
    Some(t)
}

fn marker(id: &str, at: u64) -> String {
    format!("[schedule {id} fired {at}]")
}

impl Schedule {
    /// The list as the last `schedule` result left it, minus what already fired.
    pub fn from_history(history: &[Message]) -> Arc<Self> {
        let mut items: Vec<Item> = history
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::ToolResult {
                    tool_name,
                    details: Some(d),
                    ..
                } if tool_name == "schedule" => serde_json::from_value(d["schedule"].clone()).ok(),
                _ => None,
            })
            .unwrap_or_default();
        // Firings are saved reminders: drop fired one-shots, move recurring ones on.
        let text: String = history
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => Some(
                    content
                        .iter()
                        .filter_map(|b| match b {
                            novacode_types::Block::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        items.retain_mut(|it| {
            let fired = text
                .match_indices(&format!("[schedule {} fired ", it.id))
                .filter_map(|(i, p)| text[i + p.len()..].split(']').next()?.parse::<u64>().ok())
                .max();
            match (fired, it.every_ms) {
                (Some(_), None) => false,
                (Some(at), Some(every)) => {
                    it.next_ms = it.next_ms.max(at + every);
                    true
                }
                (None, _) => true,
            }
        });
        let next = items
            .iter()
            .filter_map(|i| i.id.trim_start_matches('s').parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        Arc::new(Schedule {
            items: Mutex::new(items),
            next_id: Mutex::new(next),
            wake: Arc::new(Notify::new()),
        })
    }

    /// Fire due items into the session as waking reminders, until it ends.
    pub fn run(self: &Arc<Self>, slot: HubSlot) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(s) = me.upgrade() {
                let now = novacode_core::session::now_ms();
                let next = s.items.lock().unwrap().iter().map(|i| i.next_ms).min();
                let due: Vec<Item> = {
                    let mut g = s.items.lock().unwrap();
                    let due: Vec<Item> = g.iter().filter(|i| i.next_ms <= now).cloned().collect();
                    g.retain_mut(|i| {
                        if i.next_ms > now {
                            return true;
                        }
                        match i.every_ms {
                            // Missed runs collapse into one; the next is in the future.
                            Some(e) => {
                                while i.next_ms <= now {
                                    i.next_ms += e;
                                }
                                true
                            }
                            None => false,
                        }
                    });
                    due
                };
                if !due.is_empty() {
                    let Some(h) = slot.0.get().and_then(|w| w.upgrade()) else {
                        // Not live yet: try again shortly.
                        s.items.lock().unwrap().extend(due);
                        drop(s);
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    };
                    for i in due {
                        h.handle.emit(novacode_types::AgentEvent::Stream(
                            novacode_types::StreamEvent::Notice {
                                text: format!("⏰ schedule {}: {}", i.id, i.message),
                            },
                        ));
                        h.handle.remind_as(
                            "schedule",
                            format!("{} Scheduled message: {}", marker(&i.id, now), i.message),
                            true,
                        );
                    }
                    continue;
                }
                let sleep = next.map_or(3_600_000, |n| n.saturating_sub(now)).min(3_600_000);
                // Only the Notify is held while sleeping, so a closed session drops the list.
                let wake = s.wake.clone();
                drop(s);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(sleep)) => {}
                    _ = wake.notified() => {}
                }
            }
        });
    }

    fn list(&self) -> String {
        let g = self.items.lock().unwrap();
        if g.is_empty() {
            return "(nothing scheduled)".into();
        }
        let now = novacode_core::session::now_ms();
        g.iter()
            .map(|i| {
                let left = i.next_ms.saturating_sub(now) / 1000;
                let when = if left >= 3600 {
                    format!("in {}h{:02}m", left / 3600, left % 3600 / 60)
                } else {
                    format!("in {}m{:02}s", left / 60, left % 60)
                };
                let every = i.every_ms.map(|e| format!(", every {}s", e / 1000)).unwrap_or_default();
                format!("{} {when}{every}: {}", i.id, i.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub struct ScheduleTool(pub Arc<Schedule>);

impl Tool for ScheduleTool {
    fn name(&self) -> &str {
        "schedule"
    }
    /// Rarely needed: found through tool_search, so it costs no tokens per call.
    fn exposure(&self) -> novacode_tools::Exposure {
        novacode_tools::Exposure::Deferred
    }
    fn description(&self) -> &str {
        "Send yourself a message later, inside this session: add {message, in?: \"10m\" | at?: \"14:30\", every?: \"30m\"}, list, cancel {id}. \
         It starts a turn when due, and fires on resume if the session was closed. For checking back on long jobs or periodic tasks."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "cancel"]},
                "message": {"type": "string", "description": "What to do when it fires (written to your future self)."},
                "in": {"type": "string", "description": "Delay: 90s, 10m, 2h, 1h30m."},
                "at": {"type": "string", "description": "Local time HH:MM (next occurrence)."},
                "every": {"type": "string", "description": "Repeat interval, e.g. 30m."},
                "id": {"type": "string"}
            },
            "required": ["action"]
        })
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let s = &self.0;
            let now = novacode_core::session::now_ms();
            let get = |k: &str| args[k].as_str().filter(|v| !v.trim().is_empty());
            let r: Result<String, String> = match args["action"].as_str().unwrap_or_default() {
                "add" => (|| {
                    let message = get("message").ok_or("add needs a message")?.to_string();
                    let every_ms = match get("every") {
                        Some(e) => Some(parse_duration(e).ok_or(format!("bad interval `{e}`; use 30m, 2h…"))?),
                        None => None,
                    };
                    if every_ms.is_some_and(|e| e < 60_000) {
                        return Err("the shortest interval is 1m".into());
                    }
                    let next_ms = match (get("in"), get("at")) {
                        (Some(d), _) => now + parse_duration(d).ok_or(format!("bad delay `{d}`; use 10m, 2h…"))?,
                        (None, Some(t)) => parse_at(t, now).ok_or(format!("bad time `{t}`; use HH:MM"))?,
                        (None, None) => now + every_ms.ok_or("add needs `in`, `at` or `every`")?,
                    };
                    let id = {
                        let mut n = s.next_id.lock().unwrap();
                        *n += 1;
                        format!("s{}", *n - 1)
                    };
                    s.items.lock().unwrap().push(Item {
                        id: id.clone(),
                        message,
                        next_ms,
                        every_ms,
                    });
                    s.wake.notify_one();
                    Ok(format!("scheduled {id}"))
                })(),
                "list" => Ok(String::new()),
                "cancel" => {
                    let id = get("id").unwrap_or_default();
                    let mut g = s.items.lock().unwrap();
                    let before = g.len();
                    g.retain(|i| i.id != id);
                    if g.len() < before {
                        Ok(format!("cancelled {id}"))
                    } else {
                        Err(format!("no schedule `{id}`"))
                    }
                }
                other => Err(format!("unknown action `{other}`")),
            };
            match r {
                Ok(head) => {
                    let list = s.list();
                    let items = s.items.lock().unwrap().clone();
                    ToolOutput {
                        details: Some(json!({ "schedule": items })),
                        ..ToolOutput::text(if head.is_empty() {
                            list
                        } else {
                            format!("{head}\n{list}")
                        })
                    }
                }
                Err(e) => ToolOutput::error(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::Block;

    #[test]
    fn durations_and_resume() {
        assert_eq!(parse_duration("1h30m"), Some(5_400_000));
        assert_eq!(parse_duration("90s"), Some(90_000));
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("5x"), None);
        let items = json!({"schedule": [
            {"id": "s1", "message": "once", "next_ms": 1000},
            {"id": "s2", "message": "every", "next_ms": 1000, "every_ms": 60000},
            {"id": "s3", "message": "later", "next_ms": 9_000_000_000_000u64}
        ]});
        let history = vec![
            Message::ToolResult {
                call_id: "c".into(),
                tool_name: "schedule".into(),
                content: vec![],
                is_error: false,
                details: Some(items),
            },
            Message::User {
                content: vec![Block::text(format!(
                    "<system-reminder>{} x</system-reminder>",
                    marker("s1", 5000)
                ))],
            },
            Message::User {
                content: vec![Block::text(marker("s2", 7000))],
            },
        ];
        let s = Schedule::from_history(&history);
        let g = s.items.lock().unwrap();
        let ids: Vec<(&str, u64)> = g.iter().map(|i| (i.id.as_str(), i.next_ms)).collect();
        assert_eq!(
            ids,
            vec![("s2", 67_000), ("s3", 9_000_000_000_000)],
            "fired once-only dropped; recurring moved on"
        );
        assert_eq!(*s.next_id.lock().unwrap(), 4);
    }
}
