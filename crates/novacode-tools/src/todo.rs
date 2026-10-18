//! `todo` (L5.12): a small session task list. The list rides in each result's
//! UI-only `details`, which the session stores, so a resumed session gets it back
//! from history ([`Todo::from_history`]) with no entry kind of its own.

use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use novacode_types::Message;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Active,
    Done,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: u32,
    pub text: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Unix ms it was first started and finished (the panel shows the time spent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_ms: Option<u64>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Default)]
pub struct Todo {
    items: Mutex<Vec<Item>>,
}

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Replace the whole list with these items.
    set: Option<Vec<String>>,
    /// Append items.
    add: Option<Vec<String>>,
    /// Ids finished.
    done: Option<Vec<u32>>,
    /// Ids cancelled.
    drop: Option<Vec<u32>>,
    /// The id being worked on now (only one is active).
    start: Option<u32>,
    /// What the started item is doing now.
    note: Option<String>,
}

pub fn render(items: &[Item]) -> String {
    if items.is_empty() {
        return "(no tasks)".into();
    }
    let done = items.iter().filter(|i| i.status == Status::Done).count();
    let live = items.iter().filter(|i| i.status != Status::Dropped).count();
    let mut s = format!("{done}/{live} done\n");
    for i in items {
        let mark = match i.status {
            Status::Pending => "○",
            Status::Active => "◐",
            Status::Done => "✓",
            Status::Dropped => "✗",
        };
        s += &format!("{mark} #{} {}", i.id, i.text);
        if let Some(n) = &i.note {
            s += &format!(" ({n})");
        }
        s.push('\n');
    }
    s.trim_end().into()
}

impl Todo {
    /// The list as the last `todo` result in `history` left it.
    pub fn from_history(history: &[Message]) -> Self {
        let items = history
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::ToolResult {
                    tool_name,
                    details: Some(d),
                    ..
                } if tool_name == "todo" => serde_json::from_value::<Vec<Item>>(d["todo"].clone()).ok(),
                _ => None,
            })
            .unwrap_or_default();
        Todo {
            items: Mutex::new(items),
        }
    }

    fn apply(&self, a: Args) -> Result<Vec<Item>, String> {
        let mut g = self.items.lock().unwrap();
        let mut v = g.clone();
        let next = |v: &Vec<Item>| v.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        if let Some(set) = a.set {
            v = set
                .into_iter()
                .enumerate()
                .map(|(k, text)| Item {
                    id: k as u32 + 1,
                    text,
                    status: Status::Pending,
                    note: None,
                    started_ms: None,
                    done_ms: None,
                })
                .collect();
        }
        for text in a.add.unwrap_or_default() {
            let id = next(&v);
            v.push(Item {
                id,
                text,
                status: Status::Pending,
                note: None,
                started_ms: None,
                done_ms: None,
            });
        }
        let find = |v: &mut Vec<Item>, id: u32, st: Status| -> Result<(), String> {
            let i = v.iter_mut().find(|i| i.id == id).ok_or(format!("no task #{id}"))?;
            match st {
                Status::Active => i.started_ms = i.started_ms.or(Some(now_ms())),
                Status::Done => i.done_ms = Some(now_ms()),
                _ => {}
            }
            i.status = st;
            i.note = None;
            Ok(())
        };
        for id in a.done.unwrap_or_default() {
            find(&mut v, id, Status::Done)?;
        }
        for id in a.drop.unwrap_or_default() {
            find(&mut v, id, Status::Dropped)?;
        }
        if let Some(id) = a.start {
            for i in v.iter_mut().filter(|i| i.status == Status::Active) {
                i.status = Status::Pending;
                i.note = None;
            }
            find(&mut v, id, Status::Active)?;
            v.iter_mut().find(|i| i.id == id).unwrap().note = a.note.filter(|n| !n.trim().is_empty());
        }
        *g = v.clone();
        Ok(v)
    }
}

impl Tool for Todo {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> &str {
        "Plan and track multi-step work. One call can set (replace the list), add, mark done or drop ids, and start one id with a note. A call with no fields shows the list."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Use `todo` for work with 3 or more steps; mark an item done only when its check passes, and finish one item and start the next in the same call.",
        ]
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            match self.apply(a) {
                Ok(v) => ToolOutput {
                    details: Some(json!({ "todo": v })),
                    ..ToolOutput::text(render(&v))
                },
                Err(e) => ToolOutput::error(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};

    #[tokio::test]
    async fn set_start_done_and_resume() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let t = Todo::default();
        t.execute(json!({"set": ["read", "build", "test"]}), &c).await;
        let o = t
            .execute(json!({"done": [1], "start": 2, "note": "compiling"}), &c)
            .await;
        assert_eq!(text(&o), "1/3 done\n✓ #1 read\n◐ #2 build (compiling)\n○ #3 test");
        let o = t.execute(json!({"start": 3, "drop": [2], "add": ["ship"]}), &c).await;
        assert!(
            text(&o).starts_with("0/3 done") || text(&o).starts_with("1/3 done"),
            "{}",
            text(&o)
        );
        assert!(t.execute(json!({"done": [9]}), &c).await.is_error);
        // Resume: the stored result's details bring the list back.
        let history = vec![Message::ToolResult {
            call_id: "c".into(),
            tool_name: "todo".into(),
            content: o.content.clone(),
            is_error: false,
            details: o.details.clone(),
        }];
        let t2 = Todo::from_history(&history);
        assert_eq!(text(&t2.execute(json!({}), &c).await), text(&o));
    }
}
