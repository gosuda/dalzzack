//! Which commands this window is running right now.
//!
//! The daemon serves every connection on its own task, so several commands can
//! be in flight at once, and only the daemon knows when each one starts and
//! ends. This is the one record of that: the commands running, and the last one
//! that finished. The toolbar reads it; nothing else does.
//!
//! What it holds is a *label*, never the arguments. The CLI builds the label
//! from the command line the agent typed, with typed values already replaced
//! (see `crate::cli::label`), so a password filled into a form cannot reach a
//! window, a screenshot of it, or a screen recording.

use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

struct Running {
    id: u64,
    label: String,
    started_ms: u64,
}

struct Finished {
    label: String,
    ok: bool,
    took_ms: u64,
}

struct Tracker {
    next_id: u64,
    running: Vec<Running>,
    last: Option<Finished>,
}

impl Tracker {
    const fn new() -> Self {
        Tracker {
            next_id: 0,
            running: Vec::new(),
            last: None,
        }
    }

    fn begin(&mut self, label: String, now_ms: u64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.running.push(Running {
            id,
            label,
            started_ms: now_ms,
        });
        id
    }

    fn finish(&mut self, id: u64, ok: bool, now_ms: u64) {
        if let Some(at) = self.running.iter().position(|r| r.id == id) {
            let done = self.running.remove(at);
            self.last = Some(Finished {
                label: done.label,
                ok,
                took_ms: now_ms.saturating_sub(done.started_ms),
            });
        }
    }

    /// A command whose task was dropped before it answered. It is neither a
    /// success nor a failure, so it leaves `last` alone.
    fn abandon(&mut self, id: u64) {
        self.running.retain(|r| r.id != id);
    }

    /// The oldest running command is the one shown, and the rest are counted:
    /// the oldest is the one a person waiting on the window is waiting for.
    fn snapshot(&self) -> Value {
        if let Some(first) = self.running.first() {
            return json!({
                "state": "running",
                "cmd": first.label,
                "more": self.running.len() - 1,
                "since": first.started_ms,
            });
        }
        match &self.last {
            Some(last) => json!({
                "state": if last.ok { "done" } else { "failed" },
                "cmd": last.label,
                "more": 0,
                "took": last.took_ms,
            }),
            None => json!({ "state": "idle", "cmd": "", "more": 0 }),
        }
    }
}

static TRACKER: Mutex<Tracker> = Mutex::new(Tracker::new());
static ON_CHANGE: OnceLock<fn()> = OnceLock::new();

/// Called after every change, from whichever thread made it. The toolbar
/// registers itself here, so this module needs to know nothing about windows.
pub fn set_listener(listener: fn()) {
    let _ = ON_CHANGE.set(listener);
}

fn changed() {
    if let Some(listener) = ON_CHANGE.get() {
        listener();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn tracker() -> std::sync::MutexGuard<'static, Tracker> {
    // A panic elsewhere must not stop the window from saying what it is doing.
    TRACKER.lock().unwrap_or_else(|e| e.into_inner())
}

/// The state the toolbar shows: `state` is `idle`, `running`, `done` or
/// `failed`; `cmd` the label; `more` how many other commands are running;
/// `since` (running) the start in epoch ms; `took` (done, failed) the duration.
pub fn snapshot() -> Value {
    tracker().snapshot()
}

/// A command is running until its guard is finished or dropped.
#[must_use = "a command counts as running only while its guard lives"]
pub struct Guard {
    id: u64,
    settled: bool,
}

pub fn begin(label: impl Into<String>) -> Guard {
    let id = tracker().begin(label.into(), now_ms());
    changed();
    Guard { id, settled: false }
}

impl Guard {
    pub fn finish(mut self, ok: bool) {
        self.settled = true;
        tracker().finish(self.id, ok, now_ms());
        changed();
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.settled {
            tracker().abandon(self.id);
            changed();
        }
    }
}

/// A tool reports its own failure inside `Ok`, as `status: "failed"`.
pub fn succeeded(result: &anyhow::Result<Value>) -> bool {
    matches!(result, Ok(output) if output["status"] != "failed")
}

/// Run `work` as the command `label`, and record how it ended.
pub async fn track<F>(label: String, work: F) -> anyhow::Result<Value>
where
    F: std::future::Future<Output = anyhow::Result<Value>>,
{
    let guard = begin(label);
    let result = work.await;
    guard.finish(succeeded(&result));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(v: &Value, key: &str) -> String {
        v[key].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn a_fresh_window_is_idle() {
        let t = Tracker::new();
        assert_eq!(t.snapshot()["state"], "idle");
    }

    #[test]
    fn the_oldest_running_command_is_shown_and_the_rest_counted() {
        let mut t = Tracker::new();
        let a = t.begin("wait-for --selector .ready".into(), 1_000);
        t.begin("click --text OK".into(), 2_000);
        let s = t.snapshot();
        assert_eq!(s["state"], "running");
        assert_eq!(text(&s, "cmd"), "wait-for --selector .ready");
        assert_eq!(s["more"], 1);
        assert_eq!(s["since"], 1_000);

        // The oldest ends: the other one is now the one shown.
        t.finish(a, true, 3_000);
        let s = t.snapshot();
        assert_eq!(text(&s, "cmd"), "click --text OK");
        assert_eq!(s["more"], 0);
    }

    #[test]
    fn the_last_command_stays_after_it_ends_with_its_duration() {
        let mut t = Tracker::new();
        let id = t.begin("click --selector #pay".into(), 10_000);
        t.finish(id, false, 13_200);
        let s = t.snapshot();
        assert_eq!(s["state"], "failed");
        assert_eq!(text(&s, "cmd"), "click --selector #pay");
        assert_eq!(s["took"], 3_200);

        let id = t.begin("navigate".into(), 14_000);
        t.finish(id, true, 14_400);
        assert_eq!(t.snapshot()["state"], "done");
    }

    #[test]
    fn a_dropped_command_is_neither_done_nor_failed() {
        let mut t = Tracker::new();
        let kept = t.begin("click".into(), 0);
        t.finish(kept, true, 5);
        let lost = t.begin("wait-navigation".into(), 10);
        t.abandon(lost);
        let s = t.snapshot();
        assert_eq!(s["state"], "done", "the last real result is still shown");
        assert_eq!(text(&s, "cmd"), "click");
    }

    #[test]
    fn finishing_twice_or_an_unknown_id_changes_nothing() {
        let mut t = Tracker::new();
        let id = t.begin("click".into(), 0);
        t.finish(id, true, 5);
        t.finish(id, false, 9);
        t.finish(999, false, 9);
        assert_eq!(t.snapshot()["state"], "done");
    }

    #[test]
    fn a_tool_failure_inside_ok_is_a_failure() {
        assert!(succeeded(&Ok(json!({ "status": "completed" }))));
        assert!(!succeeded(&Ok(json!({ "status": "failed" }))));
        assert!(!succeeded(&Err(anyhow::anyhow!("boom"))));
    }
}
