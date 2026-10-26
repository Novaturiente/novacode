//! Process manager (L5.11), modelled on pi-processes: long-running commands
//! with log files, stdin, watches and exit notices. bash runs every command
//! through it hidden; a timeout or Ctrl+Alt+B just makes the process visible (L5.8).
//! Every process this session started is killed when the manager drops.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, broadcast};

/// What a notice does to the agent (pi-processes' attention levels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    /// Start a turn (or reach the running one at its next model call).
    #[default]
    Wake,
    /// Added to the context at the next model call; no turn of its own.
    Context,
    Ignore,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Watch {
    /// Text to look for in new output lines (regex with `regex: true`).
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub on: Attention,
    /// Notify on every match, not just the first.
    #[serde(default)]
    pub repeat: bool,
}

/// How a process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct End {
    pub at_ms: u64,
    /// `None` = killed by a signal.
    pub code: Option<i32>,
    /// Stopped through the manager (stop, session exit).
    pub stopped: bool,
}

/// What `<dir>/<id>.json` holds, also for orphan cleanup after a crash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub name: String,
    pub command: String,
    pub cwd: PathBuf,
    pub pid: u32,
    /// The novacode process that owns it.
    pub owner: u32,
    pub started_ms: u64,
    #[serde(default)]
    pub end: Option<End>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Started {
        id: String,
        name: String,
    },
    Ended {
        id: String,
        name: String,
        end: End,
    },
    /// For the agent: a watch matched, it exited, it stalled…
    Notice {
        id: String,
        text: String,
        on: Attention,
    },
}

struct Proc {
    info: Mutex<Info>,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    out: PathBuf,
    err: PathBuf,
    /// Hidden = a bash call still waiting on it; shown in `list` once true.
    listed: AtomicBool,
    /// Ctrl+Alt+B: the waiting bash call should return now.
    detach: Notify,
    on_exit: Mutex<Attention>,
    /// (watch, its compiled regex when `regex`, fired).
    watches: Mutex<Vec<(Watch, Option<regex::Regex>, bool)>>,
    /// Live output for a waiting bash call: (is_stderr, bytes).
    chunks: broadcast::Sender<(bool, Vec<u8>)>,
    last_output_ms: AtomicU64,
    ended: Notify,
    stopping: AtomicBool,
    /// Stall watchdog (jcode): notice after this long without output; fires once per silence.
    stall: Mutex<Option<(Duration, Attention, bool)>>,
    /// L5.8: at this time (unix ms) ask the model whether to keep it running.
    ask_at_ms: AtomicU64,
    /// Per stream (stdout, stderr): a rotation overwrote an older `.1`, so output is gone.
    dropped: [AtomicBool; 2],
    /// L6.5: the git change check's line, set when a sandboxed command exited.
    git_note: Mutex<Option<String>>,
}

/// A log file rotates into `<file>.1` past this size, so at most 2× is kept per stream.
const LOG_CAP: u64 = 8 << 20;

/// The rotated, older half of a log file.
fn older(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

fn remove_logs(p: &Proc) {
    for f in [&p.out, &p.err] {
        let _ = std::fs::remove_file(f);
        let _ = std::fs::remove_file(older(f));
    }
}

pub struct ProcessManager {
    dir: PathBuf,
    procs: Mutex<BTreeMap<String, Arc<Proc>>>,
    next: AtomicU64,
    pub events: broadcast::Sender<Event>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
}

/// Windows: each command's Job Object by pid; dropping one kills everything in it.
#[cfg(windows)]
static JOBS: Mutex<BTreeMap<u32, win32job::Job>> = Mutex::new(BTreeMap::new());

/// Put `child` and every process it starts into one Job Object (Windows), so a
/// kill reaches children whose parent already exited. Unix: process groups do this.
// shortcut: a child may start processes before it is assigned; spawn it suspended if that leaks.
pub fn adopt(child: &tokio::process::Child) {
    #[cfg(windows)]
    if let (Some(pid), Some(h)) = (child.id(), child.raw_handle()) {
        let mut info = win32job::ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        let job = win32job::Job::create_with_limit_info(&info).and_then(|j| {
            j.assign_process(h as isize)?;
            Ok(j)
        });
        match job {
            Ok(j) => {
                JOBS.lock().unwrap().insert(pid, j);
            }
            Err(e) => tracing::warn!(pid, error = %e, "no job object; kills fall back to taskkill"),
        }
    }
    #[cfg(unix)]
    let _ = child;
}

/// The command `pid` ended: free its job (Windows), which ends what it left running.
pub fn release(pid: u32) {
    #[cfg(windows)]
    JOBS.lock().unwrap().remove(&pid);
    #[cfg(unix)]
    let _ = pid;
}

// Windows has no SIGTERM for console programs: stop is a hard kill.
#[cfg(windows)]
fn kill_group(pid: u32) {
    kill_group_hard(pid);
}

/// SIGKILL a process group at once (MCP servers on shutdown).
pub fn kill_group_now(pid: u32) {
    kill_group_hard(pid);
}

#[cfg(unix)]
fn kill_group_hard(pid: u32) {
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// Drops `pid`'s Job Object; without one (an orphan of an earlier novacode),
/// `taskkill /T` kills `pid` and the children it can still see.
#[cfg(windows)]
fn kill_group_hard(pid: u32) {
    if JOBS.lock().unwrap().remove(&pid).is_some() {
        return;
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The process group `pid` leads is still alive.
#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(windows)]
fn group_alive(pid: u32) -> bool {
    match JOBS.lock().unwrap().get(&pid) {
        Some(j) => j.query_process_id_list().is_ok_and(|l| !l.is_empty()),
        None => pid_alive(pid),
    }
}

/// A process with this pid exists.
#[cfg(target_os = "linux")]
pub fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

// No `/proc` on macOS: signal 0 probes; EPERM means it exists under another user.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn pid_alive(pid: u32) -> bool {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(e) => e == nix::errno::Errno::EPERM,
    }
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/NH", "/FO", "CSV", "/FI", &format!("PID eq {pid}")])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
}

impl ProcessManager {
    /// `root` = `~/.novacode/processes`; this session's files go in `root/<session>`.
    /// Kills groups left running by novacode processes that died (orphans).
    pub fn new(root: &Path, session: &str) -> Arc<Self> {
        reap_orphans(root);
        Arc::new(ProcessManager {
            dir: root.join(session),
            procs: Mutex::default(),
            next: AtomicU64::new(1),
            events: broadcast::channel(256).0,
        })
    }

    /// Start `command` with `bash -c` in its own process group, stdin piped;
    /// inside `sandbox` when given (L6.5).
    pub fn start(
        self: &Arc<Self>,
        name: &str,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        listed: bool,
        sandbox: Option<&crate::sandbox::Sandbox>,
    ) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let id = format!("proc_{}", self.next.fetch_add(1, Ordering::Relaxed));
        // L6.5: git's setup at start, compared when it exits.
        let git = sandbox.and_then(|s| s.git_snapshot());
        let mut child = crate::sandbox::command(sandbox, command, cwd)?
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        adopt(&child);
        let pid = child.id().unwrap_or(0);
        let info = Info {
            id: id.clone(),
            name: name.to_string(),
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            pid,
            owner: std::process::id(),
            started_ms: now_ms(),
            end: None,
        };
        let p = Arc::new(Proc {
            out: self.dir.join(format!("{id}.out")),
            err: self.dir.join(format!("{id}.err")),
            info: Mutex::new(info),
            stdin: tokio::sync::Mutex::new(child.stdin.take()),
            listed: AtomicBool::new(listed),
            detach: Notify::new(),
            on_exit: Mutex::new(Attention::Wake),
            watches: Mutex::default(),
            chunks: broadcast::channel(1024).0,
            last_output_ms: AtomicU64::new(now_ms()),
            ended: Notify::new(),
            stopping: AtomicBool::new(false),
            stall: Mutex::new(None),
            ask_at_ms: AtomicU64::new(0),
            dropped: Default::default(),
            git_note: Mutex::new(None),
        });
        tokio::spawn(ticker(Arc::downgrade(self), p.clone()));
        self.procs.lock().unwrap().insert(id.clone(), p.clone());
        self.save(&p);
        if listed {
            let _ = self.events.send(Event::Started {
                id: id.clone(),
                name: name.into(),
            });
        }
        let pumps: Vec<_> = [
            (
                false,
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            ),
            (
                true,
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            ),
        ]
        .into_iter()
        .filter_map(|(e, r)| r.map(|r| tokio::spawn(pump(Arc::downgrade(self), p.clone(), e, r))))
        .collect();
        // Weak: the manager must be able to drop (and kill its groups) while processes run.
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            let status = child.wait().await;
            // Background children may hold the pipes; don't wait on them forever.
            let drain = futures_util::future::join_all(pumps);
            let _ = tokio::time::timeout(Duration::from_millis(500), drain).await;
            release(pid);
            // Before `end` is set: a waiting bash call reads the line once it sees the end.
            let git_note = git.and_then(|g| g.changes());
            *p.git_note.lock().unwrap() = git_note.clone();
            let stopped = p.stopping.load(Ordering::Relaxed);
            let end = End {
                at_ms: now_ms(),
                code: status.ok().and_then(|s| s.code()),
                stopped,
            };
            p.info.lock().unwrap().end = Some(end.clone());
            let Some(me) = me.upgrade() else { return };
            me.save(&p);
            p.ended.notify_waiters();
            let (id, name) = {
                let i = p.info.lock().unwrap();
                (i.id.clone(), i.name.clone())
            };
            if p.listed.load(Ordering::Relaxed) {
                let _ = me.events.send(Event::Ended {
                    id: id.clone(),
                    name: name.clone(),
                    end: end.clone(),
                });
                let on = *p.on_exit.lock().unwrap();
                if on != Attention::Ignore && !end.stopped {
                    let how = match end.code {
                        Some(0) => "exited successfully".to_string(),
                        Some(c) => format!("exited with code {c}"),
                        None => "was killed by a signal".to_string(),
                    };
                    let tail = tail_text(&p, 10, None);
                    let _ = me.events.send(Event::Notice {
                        id: id.clone(),
                        text: format!("Process {id} ({name}) {how}. Last output:\n{tail}"),
                        on,
                    });
                }
                if let Some(n) = git_note {
                    let _ = me.events.send(Event::Notice {
                        id: id.clone(),
                        text: format!("Process {id} ({name}): {n}"),
                        on: Attention::Context,
                    });
                }
            }
        });
        Ok(id)
    }

    /// The git change check's line for a process that exited (L6.5).
    pub fn git_note(&self, id: &str) -> Option<String> {
        self.get(id)?.git_note.lock().unwrap().clone()
    }

    /// Under the map lock: a process `clear`/`clear_one` already forgot must not be
    /// written back (its exit raced the delete and resurrected the file, with a `.bak` warning).
    fn save(&self, p: &Proc) {
        let procs = self.procs.lock().unwrap();
        let info = p.info.lock().unwrap().clone();
        if !procs.contains_key(&info.id) {
            return;
        }
        let _ = novacode_base::storage::write_json(
            &self.dir.join(format!("{}.json", info.id)),
            &info,
            novacode_base::storage::Durability::Fast,
        );
        let _ = std::fs::remove_file(novacode_base::storage::backup_path(
            &self.dir.join(format!("{}.json", info.id)),
        ));
    }

    fn get(&self, id: &str) -> Option<Arc<Proc>> {
        self.procs.lock().unwrap().get(id).cloned()
    }

    /// Show a hidden process (a bash call handing it over) and set its notices.
    pub fn reveal(&self, id: &str, on_exit: Attention) {
        if let Some(p) = self.get(id)
            && !p.listed.swap(true, Ordering::Relaxed)
        {
            *p.on_exit.lock().unwrap() = on_exit;
            let name = p.info.lock().unwrap().name.clone();
            let _ = self.events.send(Event::Started { id: id.into(), name });
        }
    }

    /// Ctrl+Alt+B: every bash call still waiting returns now; its process keeps running.
    pub fn detach_all(&self) -> usize {
        let procs: Vec<_> = self.procs.lock().unwrap().values().cloned().collect();
        let mut n = 0;
        for p in procs {
            if !p.listed.load(Ordering::Relaxed) && p.info.lock().unwrap().end.is_none() {
                p.detach.notify_one();
                n += 1;
            }
        }
        n
    }

    /// Stall notice after `secs` without output (`None` = off).
    pub fn set_stall(&self, id: &str, stall: Option<(u64, Attention)>) {
        if let Some(p) = self.get(id) {
            *p.stall.lock().unwrap() = stall.map(|(s, a)| (Duration::from_secs(s.max(5)), a, false));
        }
    }

    /// L5.8: ask the model at `at_ms` whether to keep a handed-over command running.
    pub fn ask_at(&self, id: &str, at_ms: u64) {
        if let Some(p) = self.get(id) {
            p.ask_at_ms.store(at_ms, Ordering::Relaxed);
        }
    }

    pub fn set_watches(&self, id: &str, watches: Vec<Watch>, on_exit: Option<Attention>) -> bool {
        let Some(p) = self.get(id) else { return false };
        *p.watches.lock().unwrap() = watches
            .into_iter()
            .map(|w| {
                let re = w.regex.then(|| regex::Regex::new(&w.pattern).ok()).flatten();
                (w, re, false)
            })
            .collect();
        if let Some(a) = on_exit {
            *p.on_exit.lock().unwrap() = a;
        }
        true
    }

    /// Wait for a hidden process started by bash: its exit, Ctrl+Alt+B, or `timeout`.
    /// Live output goes to `on_chunk`. Returns how it ended, `None` = still running.
    pub async fn wait(&self, id: &str, timeout: Duration, mut on_chunk: impl FnMut(&[u8])) -> Option<End> {
        let p = self.get(id)?;
        let mut rx = p.chunks.subscribe();
        let ended = p.ended.notified();
        tokio::pin!(ended);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            if let Some(e) = p.info.lock().unwrap().end.clone() {
                while let Ok((_, c)) = rx.try_recv() {
                    on_chunk(&c);
                }
                return Some(e);
            }
            tokio::select! {
                c = rx.recv() => match c {
                    Ok((_, c)) => on_chunk(&c),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                },
                _ = &mut ended => {}
                _ = &mut deadline => return None,
                _ = p.detach.notified() => return None,
            }
        }
    }

    /// Stop a process group: SIGTERM, SIGKILL after 2s.
    pub async fn stop(&self, id: &str) -> Option<End> {
        let p = self.get(id)?;
        let ended = p.ended.notified();
        tokio::pin!(ended);
        let pid = {
            let i = p.info.lock().unwrap();
            if let Some(e) = &i.end {
                return Some(e.clone());
            }
            i.pid
        };
        // Its exit is asked for: not reported as a crash.
        p.stopping.store(true, Ordering::Relaxed);
        kill_group(pid);
        if tokio::time::timeout(Duration::from_secs(2), &mut ended).await.is_err() {
            kill_group_hard(pid);
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut ended).await;
        }
        p.info.lock().unwrap().end.clone()
    }

    pub async fn write(&self, id: &str, input: &str, end: bool) -> Result<(), String> {
        let p = self.get(id).ok_or("no such process")?;
        let mut g = p.stdin.lock().await;
        let stdin = g.as_mut().ok_or("its stdin is closed")?;
        stdin.write_all(input.as_bytes()).await.map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())?;
        if end {
            *g = None;
        }
        Ok(())
    }

    /// Listed processes, oldest first.
    pub fn list(&self) -> Vec<Info> {
        self.procs
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.listed.load(Ordering::Relaxed))
            .map(|p| p.info.lock().unwrap().clone())
            .collect()
    }

    pub fn running(&self) -> usize {
        self.list().iter().filter(|i| i.end.is_none()).count()
    }

    /// The last `lines` lines of its output, optionally only lines containing `filter`.
    pub fn output(&self, id: &str, lines: usize, filter: Option<&str>) -> Option<String> {
        let p = self.get(id)?;
        Some(tail_text(&p, lines, filter))
    }

    /// Forget finished processes and delete their logs.
    pub fn clear(&self) -> usize {
        let mut g = self.procs.lock().unwrap();
        let done: Vec<String> = g
            .iter()
            .filter(|(_, p)| p.listed.load(Ordering::Relaxed) && p.info.lock().unwrap().end.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for k in &done {
            if let Some(p) = g.remove(k) {
                remove_logs(&p);
                let _ = std::fs::remove_file(self.dir.join(format!("{k}.json")));
            }
        }
        done.len()
    }

    /// Forget a hidden process a bash call finished with (and its logs).
    pub fn clear_one(&self, id: &str) {
        let removed = self.procs.lock().unwrap().remove(id);
        if let Some(p) = removed {
            remove_logs(&p);
            let _ = std::fs::remove_file(self.dir.join(format!("{id}.json")));
        }
    }

    /// Kill every running group (session exit, user choice in L5.11).
    pub fn kill_all(&self) {
        for p in self.procs.lock().unwrap().values() {
            let i = p.info.lock().unwrap();
            if i.end.is_none() && i.pid > 0 {
                kill_group_hard(i.pid);
            }
        }
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        self.kill_all();
    }
}

async fn pump(
    pm: std::sync::Weak<ProcessManager>,
    p: Arc<Proc>,
    is_err: bool,
    mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
) {
    let path = if is_err { &p.err } else { &p.out };
    let mut file = match tokio::fs::OpenOptions::new().create(true).append(true).open(path).await {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut size = file.metadata().await.map_or(0, |m| m.len());
    let mut buf = vec![0u8; 16 * 1024];
    let mut partial = String::new();
    while let Ok(n) = r.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if size > 0 && size + n as u64 > LOG_CAP {
            let _ = file.flush().await;
            let old = older(path);
            let had_old = old.exists();
            // A failed rename or reopen keeps the old handle and size, so it is tried
            // again next chunk: live output and watches go on.
            if tokio::fs::rename(path, &old).await.is_ok() {
                if had_old {
                    p.dropped[is_err as usize].store(true, Ordering::Relaxed);
                }
                if let Ok(f) = tokio::fs::OpenOptions::new().create(true).append(true).open(path).await {
                    file = f;
                    size = 0;
                }
            }
        }
        let _ = file.write_all(chunk).await;
        size += n as u64;
        let _ = p.chunks.send((is_err, chunk.to_vec()));
        p.last_output_ms.store(now_ms(), Ordering::Relaxed);
        // Watches look at whole lines.
        partial.push_str(&String::from_utf8_lossy(chunk));
        while let Some(i) = partial.find('\n') {
            let line: String = partial.drain(..=i).collect();
            if let Some(pm) = pm.upgrade() {
                check_watches(&pm, &p, line.trim_end());
            }
        }
        if partial.len() > 64 * 1024 {
            partial.clear();
        }
    }
    let _ = file.flush().await;
}

async fn ticker(pm: std::sync::Weak<ProcessManager>, p: Arc<Proc>) {
    let mut t = tokio::time::interval(Duration::from_secs(5));
    loop {
        t.tick().await;
        let Some(pm) = pm.upgrade() else { return };
        let (id, name, running, started) = {
            let i = p.info.lock().unwrap();
            (i.id.clone(), i.name.clone(), i.end.is_none(), i.started_ms)
        };
        if !running {
            return;
        }
        if !p.listed.load(Ordering::Relaxed) {
            continue;
        }
        let now = now_ms();
        let ask = p.ask_at_ms.load(Ordering::Relaxed);
        if ask > 0 && now >= ask {
            p.ask_at_ms.store(0, Ordering::Relaxed);
            let mins = (now - started) / 60_000;
            let _ = pm.events.send(Event::Notice {
                id: id.clone(),
                text: format!(
                    "Process {id} ({name}) has been running for {mins} min. Keep it running (do nothing) or stop it with `process` action `stop`."
                ),
                on: Attention::Wake,
            });
        }
        let silent = Duration::from_millis(now.saturating_sub(p.last_output_ms.load(Ordering::Relaxed)));
        let mut g = p.stall.lock().unwrap();
        if let Some((after, on, fired)) = g.as_mut() {
            if silent < *after {
                *fired = false;
            } else if !*fired {
                *fired = true;
                let _ = pm.events.send(Event::Notice {
                    id: id.clone(),
                    text: format!("Process {id} ({name}) has printed nothing for {}s.", silent.as_secs()),
                    on: *on,
                });
            }
        }
    }
}

fn check_watches(pm: &ProcessManager, p: &Proc, line: &str) {
    if !p.listed.load(Ordering::Relaxed) {
        return;
    }
    let mut hits = Vec::new();
    for (w, re, fired) in p.watches.lock().unwrap().iter_mut() {
        if *fired && !w.repeat {
            continue;
        }
        let m = if w.regex {
            re.as_ref().is_some_and(|r| r.is_match(line))
        } else {
            line.contains(&w.pattern)
        };
        if m {
            *fired = true;
            if w.on != Attention::Ignore {
                hits.push((w.pattern.clone(), w.on));
            }
        }
    }
    let (id, name) = {
        let i = p.info.lock().unwrap();
        (i.id.clone(), i.name.clone())
    };
    for (pat, on) in hits {
        let _ = pm.events.send(Event::Notice {
            id: id.clone(),
            text: format!("Process {id} ({name}) printed a line matching `{pat}`:\n{line}"),
            on,
        });
    }
}

/// The last `n` lines of stdout and stderr (stderr marked), from the log files.
fn tail_text(p: &Proc, n: usize, filter: Option<&str>) -> String {
    let read_tail = |p: &Path| -> Vec<String> {
        let Ok(mut f) = std::fs::File::open(p) else {
            return vec![];
        };
        let len = f.metadata().map_or(0, |m| m.len());
        // shortcut: only the last 1 MB is scanned; a filter over older output misses it.
        let start = len.saturating_sub(1 << 20);
        let _ = std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(start));
        let mut s = String::new();
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut f, &mut bytes);
        s.push_str(&String::from_utf8_lossy(&bytes));
        s.lines()
            .filter(|l| filter.is_none_or(|f| l.contains(f)))
            .map(String::from)
            .collect()
    };
    // Current file first; the rotated `.1` only when that is short of `n` lines.
    let read = |f: &Path, dropped: &AtomicBool| -> (Vec<String>, bool) {
        let mut v = read_tail(f);
        if v.len() >= n {
            return (v, false);
        }
        let mut old = read_tail(&older(f));
        old.append(&mut v);
        (old, dropped.load(Ordering::Relaxed))
    };
    let (o, o_dropped) = read(&p.out, &p.dropped[0]);
    let (e, e_dropped) = read(&p.err, &p.dropped[1]);
    let take = |v: Vec<String>, dropped: bool| {
        let k = v.len().saturating_sub(n);
        // Mark the gap only when the window reaches the oldest line still kept.
        let mark = if dropped && k == 0 {
            "[older output discarded]\n"
        } else {
            ""
        };
        format!("{mark}{}", v[k..].join("\n"))
    };
    match (o.is_empty(), e.is_empty()) {
        (true, true) => "(no output)".into(),
        (false, true) => take(o, o_dropped),
        (true, false) => format!("[stderr]\n{}", take(e, e_dropped)),
        (false, false) => format!("{}\n[stderr]\n{}", take(o, o_dropped), take(e, e_dropped)),
    }
}

/// After a crash: kill groups whose novacode owner is gone, and mark them ended.
fn reap_orphans(root: &Path) {
    let owner_alive = pid_alive;
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let path = f.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(Some(mut info)) = novacode_base::storage::read_json::<Info>(&path) else {
                continue;
            };
            if info.end.is_some() || owner_alive(info.owner) {
                continue;
            }
            if info.pid > 0 && group_alive(info.pid) {
                tracing::warn!(id = info.id, pid = info.pid, "killing an orphaned process group");
                kill_group_hard(info.pid);
            }
            info.end = Some(End {
                at_ms: now_ms(),
                code: None,
                stopped: true,
            });
            let _ = novacode_base::storage::write_json(&path, &info, novacode_base::storage::Durability::Fast);
            let _ = std::fs::remove_file(novacode_base::storage::backup_path(&path));
        }
    }
}

/// The `process` tool (L5.11).
pub struct ProcessTool;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Action {
    Start,
    List,
    Output,
    Write,
    Stop,
    Watch,
    Clear,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Args {
    action: Action,
    /// start: a short name, e.g. "dev server".
    name: Option<String>,
    /// start: the shell command.
    command: Option<String>,
    /// start: working directory (default: the session's).
    cwd: Option<String>,
    /// output/write/stop/watch: the process id, e.g. proc_3.
    id: Option<String>,
    /// output: lines to show from the end (default 100).
    lines: Option<usize>,
    /// output: only lines containing this text.
    filter: Option<String>,
    /// write: text for its stdin (add "\n" to end a line).
    input: Option<String>,
    /// write: close stdin afterwards.
    #[serde(default)]
    end: bool,
    /// start/watch: what its exit does: wake (default), context or ignore.
    on_exit: Option<Attention>,
    /// start/watch: output lines to be told about (replaces earlier watches).
    watches: Option<Vec<Watch>>,
    /// start/watch: be told when it prints nothing for this many seconds.
    stall_seconds: Option<u64>,
    /// start: run outside the sandbox, only for a command that needs what it hides or blocks.
    #[serde(default)]
    unsandboxed: bool,
}

fn fmt_info(i: &Info) -> String {
    let state = match &i.end {
        None => "running".to_string(),
        Some(End { stopped: true, .. }) => "stopped".into(),
        Some(End { code: Some(c), .. }) => format!("exited {c}"),
        Some(End { code: None, .. }) => "killed".into(),
    };
    format!(
        "{} {} [{state}] pid {} · {} · {}",
        i.id,
        i.name,
        i.pid,
        i.command,
        i.cwd.display()
    )
}

impl crate::Tool for ProcessTool {
    fn name(&self) -> &str {
        "process"
    }
    fn description(&self) -> &str {
        "Run and manage long-running commands (dev servers, watchers, builds) that keep going across turns. Actions: start, list, output, write (stdin), stop, watch (exit/log-line/stall notices), clear (forget finished). Every process wakes you when it exits (on_exit defaults to wake, also for bash commands moved here), and when a watched line appears: don't call watch just to be woken, and don't poll output or sleep. Everything is stopped when the session ends."
    }
    fn schema(&self) -> serde_json::Value {
        crate::schema_for::<Args>()
    }
    fn guidelines(&self) -> &[&str] {
        &[
            "Use `process` start for servers and watchers instead of `&`, nohup or setsid in bash; then carry on, notices bring you back.",
        ]
    }
    fn large_output(&self) -> bool {
        true
    }
    fn keep(&self) -> crate::Keep {
        crate::Keep::Tail
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a crate::ToolCtx,
    ) -> futures_util::future::BoxFuture<'a, crate::ToolOutput> {
        use crate::ToolOutput;
        Box::pin(async move {
            let a: Args = match crate::parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let Some(pm) = &ctx.processes else {
                return ToolOutput::error("the process manager is off in this session");
            };
            let need_id = || {
                a.id.clone()
                    .ok_or_else(|| ToolOutput::error("`id` is required for this action"))
            };
            // A bad regex would silently never match: refuse it before anything starts.
            if let Some(ws) = &a.watches
                && let Some((w, e)) = ws
                    .iter()
                    .filter(|w| w.regex)
                    .find_map(|w| regex::Regex::new(&w.pattern).err().map(|e| (w, e)))
            {
                return ToolOutput::error(format!("watch pattern `{}` is not a valid regex: {e}", w.pattern));
            }
            match a.action {
                Action::Start => {
                    let Some(cmd) = a.command.as_deref().filter(|c| !c.trim().is_empty()) else {
                        return ToolOutput::error("`command` is required to start a process");
                    };
                    let name = a.name.clone().unwrap_or_else(|| cmd.chars().take(40).collect());
                    let cwd = a.cwd.as_deref().map_or(ctx.cwd.clone(), |c| ctx.path(c));
                    let env = [("NOVACODE_SESSION_ID".to_string(), ctx.session_id.clone())];
                    let sb = if a.unsandboxed { None } else { ctx.env.sandbox() };
                    let id = match pm.start(&name, cmd, &cwd, &env, false, sb) {
                        Ok(id) => id,
                        Err(e) => return ToolOutput::error(format!("could not start: {e}")),
                    };
                    pm.set_watches(&id, a.watches.clone().unwrap_or_default(), None);
                    pm.set_stall(&id, a.stall_seconds.map(|s| (s, Attention::Wake)));
                    pm.reveal(&id, a.on_exit.unwrap_or_default());
                    // A moment for instant failures (bad command, port in use) to show.
                    let ended = pm.wait(&id, Duration::from_millis(1500), |_| {}).await;
                    let tail = pm.output(&id, 20, None).unwrap_or_default();
                    let failed = ended.as_ref().is_some_and(|e| e.code != Some(0));
                    let note = crate::bash::sandbox_note(ctx, a.unsandboxed, failed, &tail)
                        .map_or(String::new(), |n| format!("\n{n}"));
                    match ended {
                        Some(e) => ToolOutput::error(format!(
                            "{id} ({name}) ended at once ({}):\n{tail}{note}",
                            e.code.map_or("killed".into(), |c| format!("exit {c}"))
                        )),
                        None => ToolOutput::text(format!("started {id} ({name}). First output:\n{tail}{note}")),
                    }
                }
                Action::List => {
                    let l = pm.list();
                    if l.is_empty() {
                        return ToolOutput::text("no processes");
                    }
                    ToolOutput::text(l.iter().map(fmt_info).collect::<Vec<_>>().join("\n"))
                }
                Action::Output => {
                    let id = match need_id() {
                        Ok(i) => i,
                        Err(e) => return e,
                    };
                    match pm.output(&id, a.lines.unwrap_or(100).clamp(1, 2000), a.filter.as_deref()) {
                        Some(t) => ToolOutput::text(t),
                        None => ToolOutput::error(format!("no process {id}")),
                    }
                }
                Action::Write => {
                    let id = match need_id() {
                        Ok(i) => i,
                        Err(e) => return e,
                    };
                    match pm.write(&id, a.input.as_deref().unwrap_or(""), a.end).await {
                        Ok(()) => ToolOutput::text(format!("written to {id}")),
                        Err(e) => ToolOutput::error(e),
                    }
                }
                Action::Stop => {
                    let id = match need_id() {
                        Ok(i) => i,
                        Err(e) => return e,
                    };
                    match pm.stop(&id).await {
                        Some(_) => ToolOutput::text(format!("stopped {id}")),
                        None if pm.list().iter().any(|i| i.id == id) => ToolOutput::error(format!("{id} didn't stop")),
                        None => ToolOutput::error(format!("no process {id}")),
                    }
                }
                Action::Watch => {
                    let id = match need_id() {
                        Ok(i) => i,
                        Err(e) => return e,
                    };
                    if !pm.set_watches(&id, a.watches.clone().unwrap_or_default(), a.on_exit) {
                        return ToolOutput::error(format!("no process {id}"));
                    }
                    pm.set_stall(&id, a.stall_seconds.map(|s| (s, Attention::Wake)));
                    ToolOutput::text(format!("watches on {id} updated"))
                }
                Action::Clear => ToolOutput::text(format!("cleared {} finished processes", pm.clear())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bad_watch_regex_is_refused() {
        use crate::Tool;
        let d = tempfile::tempdir().unwrap();
        let mut c = crate::testutil::ctx(d.path());
        c.processes = Some(ProcessManager::new(d.path(), "s"));
        let w = serde_json::json!([{"pattern": "(", "regex": true}]);
        let o = ProcessTool
            .execute(
                serde_json::json!({"action": "start", "command": "sleep 5", "watches": w}),
                &c,
            )
            .await;
        assert!(o.is_error && crate::testutil::text(&o).contains("not a valid regex"));
        assert!(c.processes.as_ref().unwrap().list().is_empty(), "nothing started");
        let o = ProcessTool
            .execute(serde_json::json!({"action": "watch", "id": "p1", "watches": w}), &c)
            .await;
        assert!(o.is_error && crate::testutil::text(&o).contains("not a valid regex"));
    }

    #[tokio::test]
    async fn run_watch_stop_and_kill_on_drop() {
        let d = tempfile::tempdir().unwrap();
        let pm = ProcessManager::new(d.path(), "s");
        let mut ev = pm.events.subscribe();
        let id = pm
            .start(
                "srv",
                "echo booting; sleep 0.3; echo READY on 8080; sleep 30",
                d.path(),
                &[],
                true,
                None,
            )
            .unwrap();
        pm.set_watches(
            &id,
            vec![Watch {
                pattern: "READY".into(),
                regex: false,
                on: Attention::Context,
                repeat: false,
            }],
            None,
        );
        assert!(matches!(ev.recv().await.unwrap(), Event::Started { .. }));
        let n = tokio::time::timeout(Duration::from_secs(5), ev.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&n, Event::Notice { text, on: Attention::Context, .. } if text.contains("READY on 8080")),
            "{n:?}"
        );
        assert!(pm.output(&id, 10, None).unwrap().contains("booting"));
        let pid = pm.list()[0].pid;
        // Stopping is not a crash: no exit notice, just the end event.
        let end = pm.stop(&id).await.unwrap();
        assert!(end.stopped);
        let e = ev.recv().await.unwrap();
        assert!(matches!(e, Event::Ended { .. }), "{e:?}");
        assert!(ev.try_recv().is_err());
        // Another one: the manager dropping kills its group.
        let id2 = pm.start("sleeper", "sleep 30", d.path(), &[], true, None).unwrap();
        let pid2 = pm.list().iter().find(|i| i.id == id2).unwrap().pid;
        // The group exists once `setsid` has run in the child.
        for _ in 0..50 {
            if group_alive(pid2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(group_alive(pid2));
        drop(pm);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!group_alive(pid2) && !group_alive(pid));
    }

    #[tokio::test]
    async fn logs_rotate_and_mark_dropped_output() {
        let d = tempfile::tempdir().unwrap();
        let pm = ProcessManager::new(d.path(), "s");
        let mut ev = pm.events.subscribe();
        // ~20.2 MB of stdout: two rotations, so the first 8 MiB is dropped.
        let id = pm
            .start(
                "big",
                "sleep 0.2; head -c 20000000 /dev/zero | tr '\\0' 'x' | fold -w 100; echo; echo READY-AFTER",
                d.path(),
                &[],
                true,
                None,
            )
            .unwrap();
        pm.set_watches(
            &id,
            vec![Watch {
                pattern: "READY-AFTER".into(),
                regex: false,
                on: Attention::Context,
                repeat: false,
            }],
            Some(Attention::Ignore),
        );
        assert!(matches!(ev.recv().await.unwrap(), Event::Started { .. }));
        let n = tokio::time::timeout(Duration::from_secs(30), ev.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&n, Event::Notice { text, .. } if text.contains("READY-AFTER")),
            "{n:?}"
        );
        assert!(pm.wait(&id, Duration::from_secs(10), |_| {}).await.is_some());
        let p = pm.get(&id).unwrap();
        for f in [p.out.clone(), older(&p.out)] {
            assert!(std::fs::metadata(&f).unwrap().len() <= LOG_CAP, "{f:?}");
        }
        let last = pm.output(&id, 1, None).unwrap();
        assert_eq!(last, "READY-AFTER");
        assert!(
            pm.output(&id, 30_000, None)
                .unwrap()
                .starts_with("[older output discarded]\n")
        );
        assert_eq!(pm.clear(), 1);
        assert!(!p.out.exists() && !older(&p.out).exists());
    }

    #[tokio::test]
    async fn exit_notice_and_hidden_wait() {
        let d = tempfile::tempdir().unwrap();
        let pm = ProcessManager::new(d.path(), "s");
        let mut ev = pm.events.subscribe();
        // Hidden (bash): no events while it's waited on and finishes.
        let id = pm.start("quick", "echo hi", d.path(), &[], false, None).unwrap();
        let mut got = Vec::new();
        let end = pm
            .wait(&id, Duration::from_secs(5), |c| got.extend_from_slice(c))
            .await
            .unwrap();
        assert_eq!((end.code, String::from_utf8_lossy(&got).trim()), (Some(0), "hi"));
        assert!(ev.try_recv().is_err());
        // Timed out, then revealed: the exit wakes the agent with the tail.
        let id = pm
            .start("slow", "sleep 1; echo done; exit 4", d.path(), &[], false, None)
            .unwrap();
        assert!(pm.wait(&id, Duration::from_millis(200), |_| {}).await.is_none());
        pm.reveal(&id, Attention::Wake);
        assert!(matches!(ev.recv().await.unwrap(), Event::Started { .. }));
        assert!(matches!(ev.recv().await.unwrap(), Event::Ended { .. }));
        let n = ev.recv().await.unwrap();
        assert!(
            matches!(&n, Event::Notice { text, on: Attention::Wake, .. } if text.contains("exited with code 4") && text.contains("done")),
            "{n:?}"
        );
    }
}
