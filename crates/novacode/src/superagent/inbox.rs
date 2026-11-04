//! The inbox reader (L10.9 "Reports and the inbox").
//!
//! Tasks drop reports as `<super dir>/inbox/<task id>-<seq>.json`, written
//! through a temporary file and a rename. [`read`] returns the files to apply,
//! in order; super's code applies them, saves the board, then calls
//! [`Pending::done`] to delete each file.

use super::board::{Board, State};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

/// The envelope every report carries; the rest of the fields are kept as is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Unique report id (dedupe key).
    pub id: String,
    pub task: String,
    pub seq: u64,
    pub token: String,
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug)]
pub struct Pending {
    pub path: PathBuf,
    pub report: Report,
}

impl Pending {
    /// The report is applied and the board saved: delete its file.
    pub fn done(self) -> io::Result<()> {
        std::fs::remove_file(&self.path)
    }
}

/// The task id in a name of the form `<id>-<seq>.json`. Anything else, such
/// as `write_atomic`'s `.<name>.tmp.<pid>.<id>` files, is not a report.
fn task_of(name: &str) -> Option<&str> {
    let (id, seq) = name.strip_suffix(".json")?.rsplit_once('-')?;
    let ok = !id.is_empty() && !seq.is_empty() && seq.bytes().all(|b| b.is_ascii_digit());
    ok.then_some(id)
}

/// The reports waiting in `<super_dir>/inbox`, ordered by `seq` per task.
/// Reports already applied (by id) are deleted; files for an unknown or
/// archived task, with a wrong token, or that don't parse move to
/// `inbox/rejected/`. A file that can't be read, moved or deleted is logged and
/// skipped; only an unreadable inbox folder is an error.
pub fn read(super_dir: &Path, board: &Board) -> io::Result<Vec<Pending>> {
    let inbox = super_dir.join("inbox");
    let entries = match std::fs::read_dir(&inbox) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut pending = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(%e, "inbox: entry skipped");
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = task_of(&name) else { continue };
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let report = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Report>(&b).ok());
        let task = board.task(id);
        let why = match (&report, task) {
            (None, _) => Some("not a report"),
            (_, None) => Some("unknown task"),
            (_, Some(t)) if t.state == State::Archived => Some("archived task"),
            (Some(r), _) if r.task != id => Some("task differs from the file name"),
            (Some(r), Some(t)) if r.token != t.token => Some("wrong token"),
            _ => None,
        };
        match (why, report) {
            (None, Some(report)) => pending.push(Pending { path, report }),
            (why, _) => {
                let why = why.unwrap_or("not a report");
                tracing::warn!(file = %name, why, "inbox: report rejected");
                let rejected = inbox.join("rejected");
                if let Err(e) =
                    std::fs::create_dir_all(&rejected).and_then(|_| std::fs::rename(&path, rejected.join(&name)))
                {
                    tracing::warn!(file = %name, %e, "inbox: rejected report not moved");
                }
            }
        }
    }
    pending.sort_by(|a, b| {
        let key = |p: &Pending| (p.report.task.clone(), p.report.seq, p.path.clone());
        key(a).cmp(&key(b))
    });
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        let applied = board
            .task(&p.report.task)
            .is_some_and(|t| t.applied.contains(&p.report.id));
        if applied || !seen.insert(p.report.id.clone()) {
            tracing::debug!(id = %p.report.id, "inbox: duplicate report dropped");
            let name = p.path.display().to_string();
            if let Err(e) = p.done() {
                tracing::warn!(file = %name, %e, "inbox: duplicate report not deleted");
            }
        } else {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superagent::board::Task;
    use serde_json::json;
    use std::fs;

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup(name: &str) -> (Dir, Board) {
        let dir = std::env::temp_dir().join(format!("nc-inbox-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("inbox")).unwrap();
        let task = |id: &str, state| Task {
            id: id.into(),
            label: format!("{id}-x"),
            token: format!("tok-{id}"),
            state,
            ..Task::default()
        };
        let board = Board {
            tasks: vec![task("T1", State::Running), task("T2", State::Archived)],
            ..Board::default()
        };
        (Dir(dir), board)
    }

    fn drop_file(dir: &Path, name: &str, id: &str, task: &str, seq: u64, token: &str) {
        let body = json!({"id": id, "task": task, "seq": seq, "token": token, "state": "progress"});
        fs::write(dir.join("inbox").join(name), body.to_string()).unwrap();
    }

    fn ids(p: &[Pending]) -> Vec<&str> {
        p.iter().map(|p| p.report.id.as_str()).collect()
    }

    fn rejected(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir.join("inbox/rejected"))
            .map(|r| {
                r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn orders_by_seq_and_keeps_other_fields() {
        let (d, board) = setup("order");
        drop_file(&d.0, "T1-10.json", "r10", "T1", 10, "tok-T1");
        drop_file(&d.0, "T1-2.json", "r2", "T1", 2, "tok-T1");
        drop_file(&d.0, "T1-3.json", "r3", "T1", 3, "tok-T1");
        let got = read(&d.0, &board).unwrap();
        assert_eq!(ids(&got), ["r2", "r3", "r10"]);
        assert_eq!(got[0].report.fields["state"], "progress");
        got.into_iter().for_each(|p| p.done().unwrap());
        assert_eq!(fs::read_dir(d.0.join("inbox")).unwrap().count(), 0);
    }

    #[test]
    fn temporary_and_foreign_names_are_ignored() {
        let (d, board) = setup("tmp");
        drop_file(&d.0, ".T1-1.json.tmp.42.abc", "r1", "T1", 1, "tok-T1");
        drop_file(&d.0, "T1-1.json.bak", "r1", "T1", 1, "tok-T1");
        drop_file(&d.0, "T1-x.json", "r1", "T1", 1, "tok-T1");
        drop_file(&d.0, "notes.txt", "r1", "T1", 1, "tok-T1");
        assert!(read(&d.0, &board).unwrap().is_empty());
        assert!(rejected(&d.0).is_empty());
        assert_eq!(fs::read_dir(d.0.join("inbox")).unwrap().count(), 4);
    }

    #[test]
    fn duplicate_report_ids_are_dropped() {
        let (d, mut board) = setup("dup");
        board.tasks[0].applied.push("old".into());
        drop_file(&d.0, "T1-1.json", "old", "T1", 1, "tok-T1");
        // A branch-back can repeat a seq with a new report id: both kept.
        drop_file(&d.0, "T1-2.json", "r2", "T1", 2, "tok-T1");
        drop_file(&d.0, "T1-3.json", "r2", "T1", 3, "tok-T1");
        let got = read(&d.0, &board).unwrap();
        assert_eq!(ids(&got), ["r2"]);
        assert!(!d.0.join("inbox/T1-1.json").exists(), "applied one deleted");
        assert!(!d.0.join("inbox/T1-3.json").exists(), "repeat deleted");
        assert!(rejected(&d.0).is_empty());
    }

    #[test]
    fn misdirected_reports_are_rejected() {
        let (d, board) = setup("reject");
        drop_file(&d.0, "T1-1.json", "a", "T1", 1, "wrong");
        drop_file(&d.0, "T9-1.json", "b", "T9", 1, "tok-T9");
        drop_file(&d.0, "T2-1.json", "c", "T2", 1, "tok-T2");
        drop_file(&d.0, "T1-2.json", "d", "T2", 2, "tok-T2");
        fs::write(d.0.join("inbox/T1-3.json"), "{not json").unwrap();
        drop_file(&d.0, "T1-4.json", "ok", "T1", 4, "tok-T1");
        let got = read(&d.0, &board).unwrap();
        assert_eq!(ids(&got), ["ok"]);
        assert_eq!(
            rejected(&d.0),
            ["T1-1.json", "T1-2.json", "T1-3.json", "T2-1.json", "T9-1.json"]
        );
    }

    /// C6: a file that can't be moved aside is skipped, not an error for the whole inbox.
    #[test]
    fn one_bad_file_does_not_stop_the_others() {
        let (d, board) = setup("badfile");
        // `rejected` can't be made: a file holds its name.
        fs::write(d.0.join("inbox/rejected"), "").unwrap();
        fs::write(d.0.join("inbox/T1-1.json"), "{not json").unwrap();
        drop_file(&d.0, "T1-2.json", "ok", "T1", 2, "tok-T1");
        let got = read(&d.0, &board).unwrap();
        assert_eq!(ids(&got), ["ok"]);
    }
}
