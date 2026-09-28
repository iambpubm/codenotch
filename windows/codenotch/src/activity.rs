//! "Is it working?" for the providers the notch tracks.
//!
//! Two of the five publish a state field, so each is labelled with whatever it can honestly provide
//! (the same trade-off upstream made). The two that matter for the shape of this file are opposite
//! extremes: Cursor and the DeepSeek Harness *state* their state, and Codex, Antigravity and
//! WorkBuddy have to be inferred from what they write:
//!   - Cursor: the `composerHeaders` rows (JSON) in the editor's `state.vscdb` — `unfinishedRunAt`
//!     is set for the duration of a run and cleared when it ends; `hasBlockingPendingActions` /
//!     `hasPendingPlan` = waiting on you. This is **real state**. The database is in WAL mode, so
//!     it must be opened as a plain read-only connection (immutable ignores the WAL and shows the
//!     world as of the last checkpoint).
//!   - DeepSeek Harness: the session log opens a turn with `turn/start` and closes it with
//!     `turn/end`, and the harness writes that closing entry from a `finally`, so it lands whatever
//!     ended the turn. **Real state, and exact** — no window, no threshold. `approval/asked` with no
//!     `approval/decided` after it is the same for "waiting on you". See `dsh::read_state`.
//!   - Codex: the desktop app keeps turn state in `thread_turns` inside
//!     `~/.codex/thread_history_1.sqlite` (status = inProgress with an empty completed_at = running)
//!     — real state. The CLI / VS Code extension fall back to classifying the last entry of the
//!     rollout, with a silence threshold that depends on the entry type.
//!   - WorkBuddy: its transcripts are the same idea as a Codex rollout written by another client, so
//!     they are read the same way and with the same thresholds — the last entry says which part of a
//!     turn the session stopped in, and how long the file has been quiet says whether it is still
//!     there. No `approval` entries exist in this format at all, so it reports busy and never
//!     waiting; the notch stays silent rather than inventing a state the file cannot support.
//!   - Antigravity: transcript.jsonl is appended during a run (each step is written only once it
//!     completes, so status is always DONE and useless); written within the last 45 s = working
//!     (the model can think for a long time between steps, hence the wide window).
//!
//! ## The fifth thing the notch draws
//!
//! Upstream's `ActivitySummary.State` has four cases, not three: working, waiting, **success** and
//! idle. Success is a run that has just finished, drawn as a green pulse where the spinner was, so
//! the notch can be understood at a glance without reading it. It is also the only one of the four
//! that upstream never treats as an alarm — nothing chimes and nothing is asked of you.
//!
//! It is deliberately short-lived: upstream's own producers give it a nine-to-ten second window
//! (Kimi: `now - turnEnd <= 10`; Antigravity: `age <= 9` once the turn is over; Cursor: the first
//! nine seconds past its staleness threshold). Anything longer stops being "just now" and joins the
//! steady state, and a green ring that is always on says nothing at all. `SUCCESS_MS` is that
//! window, and each reader below is judged by whether it can honestly fill it:
//!
//!   - the harness **can**: `turn/end` carries `reason.kind`, and `completed` is the harness saying
//!     the turn finished its work. A turn that was aborted, failed or hit its token ceiling also
//!     closed, and none of those is a piece of work finishing — so none of them is drawn green.
//!   - Codex **can**: newer builds write `task_complete`, which is the same statement written in a
//!     different client's words. `turn_aborted` sits right beside it and is *not* the same thing.
//!   - Antigravity and Cursor **almost** can: neither states the outcome, so both are read the way
//!     upstream reads them — the file going quiet past the working threshold, which is what
//!     "finished" looks like from outside.
//!   - WorkBuddy **cannot**, and this is worth stating plainly because it is the one that looks like
//!     it should. Its transcript ends a turn with an ordinary `message` from the assistant, and that
//!     is exactly what mid-turn narration is written as too. Measured over the 113 transcripts on
//!     this machine: 566 assistant messages, of which **555 are followed by more tool work in the
//!     same turn**, and the gap to the next record runs to 94 s (p99 is 130 ms). Nothing separates
//!     "the answer" from "a sentence on the way to another tool call" until the file stops moving,
//!     and a file that stopped moving is not evidence that anything finished. It therefore reports
//!     busy and never success, on the same principle that keeps it out of `waiting`.
//!
//! Polled every 2 s (upstream cadence), broadcast only on change. Cost discipline: database
//! connections stay open, nothing is re-queried unless the file's mtime changed, the rollout tail
//! is re-read only when its mtime changed, and the thread runs at lowered priority. The two
//! newest readers follow the same rule — listing a tree is the expensive part, so it happens on a
//! timer, and a file is only parsed when its size or mtime says it moved.

use crate::AppState;
use serde::Serialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const INTERVAL: Duration = Duration::from_secs(2);
const ANTIGRAVITY_STALE_MS: u64 = 45_000;

/// How long a file may be quiet before its last entry stops counting as work. Three windows, one
/// per kind of step, and the same three the Codex reader has always used — the two formats say the
/// same things, so the same allowance fits both:
///   - a tool call is running: a build or an install can take minutes, so ten;
///   - the model is deciding: long reasoning has to be tolerated, so two minutes;
///   - an assistant message: short, because that record is ambiguous. Measured over the transcripts
///     on this machine, an assistant message is followed by more tool work in the same turn 555
///     times out of 566 — it is narration far more often than it is an answer — and the next record
///     lands a median 6 ms later. Four seconds covers the gap comfortably while claiming as little
///     as possible, which is the same "err short" bargain the Codex reader strikes.
const TOOL_QUIET_MS: u64 = 10 * 60_000;
const THINKING_QUIET_MS: u64 = 120_000;
const ANSWER_QUIET_MS: u64 = 4_000;

/// Ten minutes, and it does two jobs. It is the widest of the three windows above, so a transcript
/// older than this cannot be mid-turn and is dropped by the listing without being opened. And it is
/// how long a `turn/start` with no `turn/end` is believed: the pair is authoritative exactly until
/// the harness is killed, and then the log simply stops, because the `finally` that would have
/// closed the turn never ran.
const ACTIVE_MS: u64 = 10 * 60_000;

/// How long a run that has just finished keeps its green pulse — the fifth state, and the only one
/// that is an announcement rather than a state of being.
///
/// Ten seconds is upstream's own number, in all three places it produces this state: the Kimi
/// monitor allows `now - turnEnd <= 10`, Antigravity nine seconds past the end of the turn, and
/// Cursor the first nine seconds beyond its staleness threshold. Longer than that and "it just
/// finished" has stopped being true; shorter and a glance across the room misses it.
const SUCCESS_MS: u64 = 10_000;

/// How often the two session trees are re-listed. Walking directories and statting every session is
/// the expensive half of both readers, and the set of files that could be working does not change
/// anywhere near as fast as the 2 s tick.
const LIST_INTERVAL: u64 = 15_000;

#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct Activity {
    /// Provider id: codex / cursor / gemini / workbuddy / dsh
    pub provider: String,
    /// busy | waiting | success
    pub state: String,
    pub name: String,
    pub detail: String,
    /// ms epoch
    pub since: u64,
}

/// How long an activity is still worth drawing, by state. `ACTIVE_MS` for the two that describe
/// something ongoing, because the widest window is the right one when the exact per-step timing is
/// not what is being asked; `SUCCESS_MS` for the announcement, which must go away on its own.
fn window_of(state: &str) -> u64 {
    if state == "success" {
        SUCCESS_MS
    } else {
        ACTIVE_MS
    }
}

/// Which row is drawn first when a provider has more than one. Waiting outranks working because it
/// is the only state that wants something from you; a run that just finished is worth seeing before
/// a run that is still going, since the whole point of the green ring is to catch your eye; and
/// anything unrecognised sinks to the bottom.
///
/// The order is upstream's (`waiting: 0; busy: 1; success: 2; idle: 3` in `TooltipCard`), kept here
/// rather than re-derived so the two products sort a provider's rows the same way.
fn rank(state: &str) -> u8 {
    match state {
        "waiting" => 0,
        "busy" => 1,
        "success" => 2,
        _ => 3,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mtime_ms(p: &std::path::Path) -> Option<u64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}


/// Persistent connection + change gating: the query runs again only when the database file (or its
/// -wal) changed mtime; otherwise the last result is reused. Cursor's state.vscdb is over 2 GB, and
/// reopening it every 2 s for a table scan slowed the whole machine (typing lagged).
struct DbCache {
    path: std::path::PathBuf,
    conn: Option<rusqlite::Connection>,
    sig: (u64, u64),
    last: Vec<Activity>,
    checked_once: bool,
}

impl DbCache {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path, conn: None, sig: (0, 0), last: Vec::new(), checked_once: false }
    }
    fn signature(&self) -> (u64, u64) {
        let wal = {
            let mut o = self.path.as_os_str().to_owned();
            o.push("-wal");
            std::path::PathBuf::from(o)
        };
        (mtime_ms(&self.path).unwrap_or(0), mtime_ms(&wal).unwrap_or(0))
    }
    /// Calls f only when something changed (or on the first run); f returning None means the query failed → drop the connection and reopen next time
    fn refresh<F: FnOnce(&rusqlite::Connection) -> Option<Vec<Activity>>>(&mut self, f: F) -> Vec<Activity> {
        let sig = self.signature();
        if self.checked_once && sig == self.sig {
            return self.last.clone();
        }
        self.sig = sig;
        self.checked_once = true;
        if self.conn.is_none() {
            self.conn = open_ro(&self.path);
        }
        let Some(conn) = self.conn.as_ref() else {
            self.last.clear();
            return Vec::new();
        };
        match f(conn) {
            Some(v) => self.last = v,
            None => {
                self.conn = None;
                self.last.clear();
            }
        }
        self.last.clone()
    }
}

/// A file's identity for change detection. Either half moving means the file was written to, which
/// is the only signal either of the two newest readers has that anything happened at all.
fn file_sig(path: &std::path::Path) -> (u64, u64) {
    let Ok(meta) = std::fs::metadata(path) else { return (0, 0) };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    (meta.len(), mtime)
}

/// A WorkBuddy transcript the probe is following, and the parse from the last time it moved.
struct LiveTranscript {
    path: std::path::PathBuf,
    sig: (u64, u64),
    state: Option<crate::workbuddy_tokens::State>,
}

/// A harness session the probe is following, likewise. Kept in its own type rather than shared with
/// the transcript above because the two parses have nothing in common past the idea of a cache.
struct LiveSession {
    path: std::path::PathBuf,
    sig: (u64, u64),
    state: Option<crate::dsh::State>,
}

/// Everything the probe thread keeps between ticks
struct Ctx {
    cursor: DbCache,
    codex_turns: DbCache,
    codex_names: Option<rusqlite::Connection>,
    rollout_path: Option<std::path::PathBuf>,
    rollout_checked_at: u64,
    rollout_sig: u64,
    rollout_last: Vec<Activity>,
    workbuddy_live: Vec<LiveTranscript>,
    workbuddy_listed_at: u64,
    dsh_live: Vec<LiveSession>,
    dsh_listed_at: u64,
}

impl Ctx {
    fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            cursor: DbCache::new(crate::cursor::store_url().unwrap_or_default()),
            codex_turns: DbCache::new(home.join(".codex").join("thread_history_1.sqlite")),
            codex_names: None,
            rollout_path: None,
            rollout_checked_at: 0,
            rollout_sig: 0,
            rollout_last: Vec::new(),
            workbuddy_live: Vec::new(),
            workbuddy_listed_at: 0,
            dsh_live: Vec::new(),
            dsh_listed_at: 0,
        }
    }
}

// ---------------- Cursor ----------------

fn open_ro(path: &std::path::Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return None;
    }
    rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
}

fn cursor_activity(ctx: &mut Ctx) -> Vec<Activity> {
    let now = now_ms();
    ctx.cursor.refresh(|conn| {
        let mut stmt = conn.prepare("SELECT value FROM composerHeaders WHERE isArchived = 0 ORDER BY recency DESC LIMIT 40").ok()?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).ok()?;
        let mut out = Vec::new();
        for json in rows.flatten() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else { continue };
            if v.get("composerId").and_then(|x| x.as_str()).is_none() {
                continue;
            }
            let blocked = v.get("hasBlockingPendingActions").and_then(|x| x.as_bool()) == Some(true)
                || v.get("hasPendingPlan").and_then(|x| x.as_bool()) == Some(true);
            let running = v.get("unfinishedRunAt").and_then(|x| x.as_f64());
            // The conversation checkpoint, which Cursor moves on every message and every tool
            // result. Upstream names it as the only timestamp that answers "is this still going" —
            // `unfinishedRunAt` is a flag rather than a clock, and is measured against a real store
            // to hold the *composer's creation time*, not the current run's. So a row that is not
            // running and whose checkpoint moved seconds ago is a run that has just ended, which is
            // the green ring.
            //
            // `lastUpdatedAt` is deliberately **not** used as a fallback the way upstream uses it.
            // It is what `ORDER BY recency DESC` above sorts on, so it carries at least some of the
            // meaning "when was this chat last looked at", and opening a week-old chat would flash
            // green for ten seconds. Rows that carry only `lastUpdatedAt` therefore show no
            // success, which is what they showed before this existed.
            let checkpoint = v.get("conversationCheckpointLastUpdatedAt").and_then(|x| x.as_f64());
            let just_finished = !blocked
                && running.is_none()
                && checkpoint.is_some_and(|ms| now.saturating_sub(ms.max(0.0) as u64) <= SUCCESS_MS);
            let state = if blocked {
                "waiting"
            } else if running.is_some() {
                "busy"
            } else if just_finished {
                "success"
            } else {
                continue; // forty idle past conversations are not forty things happening now
            };
            let since = running
                .or(checkpoint)
                .or_else(|| v.get("lastUpdatedAt").and_then(|x| x.as_f64()))
                .or_else(|| v.get("createdAt").and_then(|x| x.as_f64()))
                .map(|ms| ms as u64)
                .unwrap_or_else(now_ms);
            out.push(Activity {
                provider: "cursor".into(),
                state: state.into(),
                name: v.get("name").and_then(|x| x.as_str()).unwrap_or("Untitled chat").to_string(),
                detail: match state {
                    "waiting" => "needs your input".into(),
                    "success" => "Complete".into(),
                    _ => v.get("subtitle").and_then(|x| x.as_str()).unwrap_or("Working").to_string(),
                },
                since,
            });
        }
        out.sort_by_key(|a| (rank(&a.state), std::cmp::Reverse(a.since)));
        Some(out)
    })
}

// ---------------- Codex ----------------

/// The last meaningful entry at the tail of a rollout says which step Codex is on.
/// Lines look like {"timestamp","type":"response_item"|"turn_context"|"event_msg"|…,"payload":{…}};
/// task_started/task_complete events are not always written, so the decision rests on the entry
/// type plus how long the file has been silent:
///   function call (a tool is running, or waiting for your approval) → busy, for up to 10 minutes;
///   tool output / user message / turn context / reasoning → the model is deciding the next step,
///   busy while silent for < 120 s (long thinking has to be tolerated);
///   assistant message → could be the final answer or narration along the way, busy while silent for < 4 s;
///   task_complete → success for 10 s; turn_aborted → nothing. Bookkeeping lines such as token_count
///   are skipped.
#[derive(Clone, Copy, PartialEq, Debug)]
enum CodexStep {
    Tool,
    Thinking,
    AsstMsg,
    /// `task_complete` — Codex saying the turn ran to its end.
    Complete,
    /// `turn_aborted` — the turn was cut short. Beside `Complete` and not the same thing: an aborted
    /// turn is not a piece of work finishing, which is the distinction upstream's rollout reader
    /// makes too ("An aborted turn is not a successful completion").
    Aborted,
}

fn codex_last_step(text: &str) -> Option<(CodexStep, u64)> {
    for line in text.lines().rev().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis().max(0) as u64)
            .unwrap_or(0);
        let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let p = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let pt = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let step = match kind {
            "turn_context" => Some(CodexStep::Thinking),
            "response_item" => match pt {
                "function_call" | "local_shell_call" | "custom_tool_call" | "web_search_call" => Some(CodexStep::Tool),
                "function_call_output" | "custom_tool_call_output" | "reasoning" => Some(CodexStep::Thinking),
                "message" => match p.get("role").and_then(|x| x.as_str()).unwrap_or("") {
                    "assistant" => Some(CodexStep::AsstMsg),
                    "user" => Some(CodexStep::Thinking),
                    _ => None, // system/developer messages say nothing about state
                },
                _ => None,
            },
            "event_msg" => match pt {
                "task_complete" => Some(CodexStep::Complete), // an explicit end of turn
                "turn_aborted" => Some(CodexStep::Aborted),
                "task_started" | "item_started" | "exec_command_begin" => Some(CodexStep::Thinking),
                "user_message" => Some(CodexStep::Thinking),
                "agent_message" => Some(CodexStep::AsstMsg),
                "agent_reasoning" | "agent_reasoning_raw_content" => Some(CodexStep::Thinking),
                _ => None, // token_count and other bookkeeping lines
            },
            _ => None,
        };
        if let Some(st) = step {
            return Some((st, ts));
        }
    }
    None
}

/// The desktop app's real state: table `thread_turns` in `~/.codex/thread_history_1.sqlite`
/// (status = inProgress / completed…, started_at in seconds, empty completed_at = still running).
/// The app maintains this turn table itself, which is far more reliable than a file mtime. Guard
/// against "inProgress forever after a crash": no new item for the thread in the last 10 minutes
/// (`thread_items.created_at_ms`) while the turn started more than 2 minutes ago → treated as stale.
fn codex_turns_in_progress(ctx: &mut Ctx) -> Vec<Activity> {
    let now = now_ms();
    if ctx.codex_names.is_none() {
        ctx.codex_names = dirs::home_dir().and_then(|h| open_ro(&h.join(".codex").join("state_5.sqlite")));
    }
    let names = ctx.codex_names.as_ref();
    ctx.codex_turns.refresh(|conn| {
        let mut stmt = conn
            .prepare("SELECT thread_id, started_at FROM thread_turns WHERE status = 'inProgress' ORDER BY started_at DESC LIMIT 8")
            .ok()?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, rusqlite::types::Value>(1)?))).ok()?;
        let mut out = Vec::new();
        for (thread_id, started) in rows.flatten() {
            let started_ms = match started {
                rusqlite::types::Value::Integer(i) => (i as u64) * if i > 10_000_000_000 { 1 } else { 1000 },
                rusqlite::types::Value::Real(f) => (f * if f > 10_000_000_000.0 { 1.0 } else { 1000.0 }) as u64,
                _ => 0,
            };
            // The thread's latest item: freshness, and whether it is waiting for approval
            let (last_ms, last_type): (Option<i64>, Option<String>) = conn
                .query_row(
                    "SELECT created_at_ms, item_type FROM thread_items WHERE thread_id = ?1 ORDER BY created_at_ms DESC LIMIT 1",
                    [&thread_id],
                    |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap_or((None, None));
            let last = last_ms.map(|v| v as u64).unwrap_or(started_ms);
            let fresh = now.saturating_sub(last) <= 10 * 60_000 || now.saturating_sub(started_ms) <= 2 * 60_000;
            if !fresh {
                continue;
            }
            let mut name = String::new();
            if let Some(c) = names {
                if let Ok((title, first, nick)) = c.query_row(
                    "SELECT COALESCE(title,''), COALESCE(first_user_message,''), COALESCE(agent_nickname,'') FROM threads WHERE id = ?1",
                    [&thread_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                ) {
                    name = if !title.trim().is_empty() {
                        title
                    } else if !first.trim().is_empty() {
                        first.chars().take(40).collect()
                    } else if !nick.trim().is_empty() {
                        format!("Agent {nick}")
                    } else {
                        String::new()
                    };
                }
            }
            if name.is_empty() {
                name = "Codex".into();
            }
            let lt = last_type.unwrap_or_default().to_lowercase();
            let waiting = lt.contains("approval") || lt.contains("permission") || lt.contains("request_user");
            out.push(Activity {
                provider: "codex".into(),
                state: if waiting { "waiting" } else { "busy" }.into(),
                name,
                detail: if waiting { "needs your input".into() } else { "Working".into() },
                since: started_ms,
            });
        }
        Some(out)
    })
}

/// When a rollout's last step happened: the record's own clock when it carries one, the file's
/// write time only as a fallback for a record that carries none.
///
/// Deliberately **not** the later of the two. A rollout goes on being written after the turn is
/// over — one on this machine has `thread_settings_applied` landing 76 s behind its
/// `task_complete` — so dating a completion from the last write would hold a green ring up long
/// after the work ended, and 90 s of `token_count` bookkeeping would keep a finished answer looking
/// busy. Split out rather than inlined because the choice is invisible in the result until the day
/// it is wrong, and it is one line to pin down with a test.
fn step_at(ts: u64, mtime: u64) -> u64 {
    if ts > 0 {
        ts
    } else {
        mtime
    }
}

/// What a rollout's last entry still means after `quiet` milliseconds of silence, or nothing at all.
///
/// Split out of the walk so each window can be tested at its own edges rather than only in the
/// middle: the boundaries are the whole content of this decision, and a window a millisecond too
/// tight looks exactly like a correct one until the day it hides work that was happening.
fn codex_showing(step: CodexStep, quiet: u64) -> Option<&'static str> {
    match step {
        // A build or an install can take minutes.
        CodexStep::Tool => (quiet <= TOOL_QUIET_MS).then_some("busy"),
        // Long reasoning has to be tolerated.
        CodexStep::Thinking => (quiet <= THINKING_QUIET_MS).then_some("busy"),
        // The final answer, or one sentence on the way to another tool call — indistinguishable
        // from here, so it errs short rather than claiming a session is busy when it is not.
        CodexStep::AsstMsg => (quiet <= ANSWER_QUIET_MS).then_some("busy"),
        // Codex said the turn was over. The only entry in a rollout that means a piece of work
        // finished, and the only one drawn green.
        CodexStep::Complete => (quiet <= SUCCESS_MS).then_some("success"),
        // An aborted turn is neither finished work nor work in progress.
        CodexStep::Aborted => None,
    }
}

fn codex_activity(ctx: &mut Ctx) -> Vec<Activity> {
    // 1. The desktop app's real state
    let turns = codex_turns_in_progress(ctx);
    if !turns.is_empty() {
        return turns;
    }
    // 2. CLI / extension: locate the rollout every 30 s; skip the 256 KB tail read when its mtime has not changed
    let now = now_ms();
    if now.saturating_sub(ctx.rollout_checked_at) > 30_000 || ctx.rollout_path.is_none() {
        ctx.rollout_checked_at = now;
        ctx.rollout_path = crate::codex::newest_rollout();
    }
    let Some(p) = ctx.rollout_path.clone() else { return vec![] };
    let mtime = mtime_ms(&p).unwrap_or(0);
    if mtime == ctx.rollout_sig {
        // Content unchanged: the last entry still stands, so only whether it is still inside its
        // window has to be re-decided. That is what retires a success when its ten seconds are up —
        // nothing writes to the file to announce the end of an announcement.
        return ctx
            .rollout_last
            .iter()
            .filter(|a| now.saturating_sub(a.since) <= window_of(&a.state))
            .cloned()
            .collect();
    }
    ctx.rollout_sig = mtime;
    ctx.rollout_last.clear();
    if let Some(text) = crate::codex::tail_text(&p) {
        if let Some((step, ts)) = codex_last_step(&text) {
            // When the step happened, from the record's own clock — see `step_at`.
            let at = step_at(ts, mtime);
            if let Some(state) = codex_showing(step, now.saturating_sub(at)) {
                ctx.rollout_last = vec![Activity {
                    provider: "codex".into(),
                    state: state.into(),
                    name: "Codex".into(),
                    detail: if state == "success" { "Complete".into() } else { "Working".into() },
                    since: at,
                }];
            }
        }
    }
    ctx.rollout_last.clone()
}

// ---------------- Antigravity ----------------

fn antigravity_activity() -> Vec<Activity> {
    let mut newest: Option<(String, u64)> = None;
    let brains = crate::antigravity::state_roots().into_iter().filter_map(|r| std::fs::read_dir(r.join("brain")).ok());
    for e in brains.flat_map(|rd| rd.flatten()) {
        let t = e.path().join(".system_generated").join("logs").join("transcript.jsonl");
        let Some(m) = mtime_ms(&t) else { continue };
        if newest.as_ref().map(|(_, n)| m > *n).unwrap_or(true) {
            newest = Some((e.file_name().to_string_lossy().to_string(), m));
        }
    }
    let Some((_, at)) = newest else { return vec![] };
    if now_ms().saturating_sub(at) > ANTIGRAVITY_STALE_MS {
        return vec![];
    }
    vec![Activity { provider: "gemini".into(), state: "busy".into(), name: "Antigravity".into(), detail: "Working".into(), since: at }]
}

// ---------------- WorkBuddy and the DeepSeek Harness ----------------

/// What to call a session: the title it gave itself when it wrote one, otherwise the last segment of
/// the directory it runs in. Both readers want the same thing said the same way, and a blank row is
/// worse than a provider name, so there is always an answer.
///
/// A bare drive root names nothing — `C:\` would otherwise come out as `C:` — so it falls through to
/// the provider name along with the empty cases.
fn session_name(title: &str, cwd: &str, fallback: &str) -> String {
    let title = title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let tail = cwd
        .trim_end_matches(|c| c == '\\' || c == '/')
        .rsplit(|c| c == '\\' || c == '/')
        .next()
        .unwrap_or("")
        .trim();
    if tail.is_empty() || tail.ends_with(':') {
        fallback.to_string()
    } else {
        tail.to_string()
    }
}

/// Whether a transcript's last entry still counts as work after `quiet` milliseconds of silence.
///
/// Split out from the walk so the thresholds can be tested at their edges rather than only in the
/// middle: the boundaries are the whole content of this decision, and a window that is one
/// millisecond too tight looks exactly like a correct one until the day it matters.
fn step_still_running(step: crate::workbuddy_tokens::Step, quiet: u64) -> bool {
    use crate::workbuddy_tokens::Step;
    match step {
        Step::Tool => quiet <= TOOL_QUIET_MS,
        Step::Thinking | Step::Asked => quiet <= THINKING_QUIET_MS,
        Step::Answered => quiet <= ANSWER_QUIET_MS,
    }
}

/// WorkBuddy: the transcripts the app appends to, read the way a Codex rollout is because they are
/// the same document written by a different client. The last entry says which part of a turn the
/// session stopped in; the quiet time since says whether it is still there.
///
/// It reports busy and never waiting or success, and both refusals come from the same place: this
/// file is written without the two records that would settle either question.
///
///   - **Never waiting.** There are no approval entries at all — WorkBuddy asks for permission in
///     its own window, not in the transcript — so a call that is only waiting on you is written
///     exactly like one that is executing. There is no honest way to tell them apart from here, and
///     a guess would put an amber "needs your input" on a cell that has nothing to say.
///   - **Never success.** Nothing in the file marks the end of a turn. The last record of a finished
///     turn is an ordinary assistant `message`, and that is what mid-turn narration is written as
///     too, so the two are indistinguishable until the file stops moving — and a file that stopped
///     moving is not evidence that anything finished. See the module doc for the measurement.
fn workbuddy_activity(ctx: &mut Ctx) -> Vec<Activity> {
    let now = now_ms();
    if now.saturating_sub(ctx.workbuddy_listed_at) > LIST_INTERVAL {
        ctx.workbuddy_listed_at = now;
        // The listing already drops anything not written to inside the widest window, so no file
        // older than a step could still be alive in is ever opened.
        ctx.workbuddy_live = crate::workbuddy_tokens::recent_transcripts(ACTIVE_MS)
            .into_iter()
            .map(|(path, _)| LiveTranscript { path, sig: (0, 0), state: None })
            .collect();
    }
    let mut out = Vec::new();
    for row in ctx.workbuddy_live.iter_mut() {
        let sig = file_sig(&row.path);
        if sig != row.sig {
            row.sig = sig;
            row.state = crate::workbuddy_tokens::read_state(&row.path);
        }
        let Some(state) = row.state.as_ref() else { continue };
        let Some((step, at)) = state.step else { continue };
        if !step_still_running(step, now.saturating_sub(at)) {
            continue;
        }
        out.push(Activity {
            provider: "workbuddy".into(),
            state: "busy".into(),
            name: session_name(&state.title, &state.cwd, "WorkBuddy"),
            detail: "Working".into(),
            since: at,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.since));
    out
}

/// What the notch should draw for one session log, or nothing at all.
///
/// Waiting outranks everything: it is the only state that wants something from you, and it is the
/// only one that does not expire — a question stays on screen until you answer it, however long
/// that takes.
///
/// The other two are timed, and they are timed off **different clocks**, which is the whole reason
/// this takes two:
///
///   - A turn still open is believed while the *file* is still moving. Its own `turn/start` time is
///     no use — a long turn started long ago — and the only way to tell a turn that is thinking
///     from one whose process was killed before its `finally` ran is whether the log is still being
///     appended to.
///   - A turn that has closed needs the opposite clock: `turn/end` is the moment the work stopped,
///     so that is what the green window counts from. Later flushes to the file say nothing about it
///     — they are why a success cannot be timed off the mtime, which keeps moving after the turn is
///     over and would hold the ring green for as long as the harness tidies up.
fn dsh_showing(state: &crate::dsh::State, file_age: u64, now: u64) -> Option<(&'static str, &'static str)> {
    if state.waiting() {
        Some(("waiting", "needs your input"))
    } else if state.working() {
        (file_age <= ACTIVE_MS).then_some(("busy", "Working"))
    } else if state.completed() && now.saturating_sub(state.at) <= SUCCESS_MS {
        Some(("success", "Complete"))
    } else {
        None
    }
}

/// The DeepSeek Harness states its own state, so this asks the log instead of consulting a table of
/// thresholds: the last of `turn/start`, `turn/end`, `approval/asked` and `approval/decided` is the
/// answer, `turn/end` is written from a `finally` so it cannot be missed, and the `reason.kind` that
/// comes with it says whether the work finished or was cut short. That last part is what makes this
/// the only provider on the machine that can be drawn green on its own say-so.
///
/// One heuristic survives, for a harness that was killed: a turn left open by a process that died
/// looks exactly like a turn left open by a process that is thinking, and the only thing that tells
/// them apart is whether the log is still being written to. Ten minutes of silence is the same
/// allowance the tool window makes, and covers a model working through one long step.
fn dsh_activity(ctx: &mut Ctx) -> Vec<Activity> {
    let now = now_ms();
    if now.saturating_sub(ctx.dsh_listed_at) > LIST_INTERVAL {
        ctx.dsh_listed_at = now;
        // Listing returns every session the machine has ever run, so the filter is what keeps this
        // off the history: a log untouched for ten minutes cannot be holding an open turn, and the
        // widest window a session can still be drawn in is `ACTIVE_MS` — the ten-second success
        // band sits inside it, so nothing that could still be shown is dropped here.
        ctx.dsh_live = crate::dsh::session_logs()
            .into_iter()
            .filter(|path| now.saturating_sub(file_sig(path).1) <= ACTIVE_MS)
            .map(|path| LiveSession { path, sig: (0, 0), state: None })
            .collect();
    }
    let mut out = Vec::new();
    for row in ctx.dsh_live.iter_mut() {
        let sig = file_sig(&row.path);
        if sig != row.sig {
            row.sig = sig;
            row.state = crate::dsh::read_state(&row.path);
        }
        let Some(state) = row.state.as_ref() else { continue };
        let Some((label, detail)) = dsh_showing(state, now.saturating_sub(sig.1), now) else { continue };
        out.push(Activity {
            provider: "dsh".into(),
            state: label.into(),
            name: session_name(&state.title, &state.cwd, "DeepSeek Harness"),
            detail: detail.into(),
            since: state.at,
        });
    }
    // Waiting, then working, then the run that just finished, newest first within each — the order
    // the card draws them in, so what gets cut by the row limit is what matters least.
    out.sort_by_key(|a| (rank(&a.state), std::cmp::Reverse(a.since)));
    out
}

// ---------------- Putting it together ----------------

#[derive(Clone, Copy, Default)]
pub struct Presence {
    cursor: bool,
    codex: bool,
    gemini: bool,
    workbuddy: bool,
    dsh: bool,
}

fn presence() -> Presence {
    Presence {
        cursor: crate::cursor::present(),
        codex: crate::codex::present(),
        gemini: crate::antigravity::present(),
        // Both of these answer for a transcript tree, not for an account, so they are present
        // exactly when there is something on disk to read — the same test the token halves make.
        workbuddy: crate::workbuddy_tokens::present(),
        dsh: crate::dsh::present(),
    }
}

fn read_all(p: Presence, ctx: &mut Ctx) -> Vec<Activity> {
    let mut all = Vec::new();
    if p.cursor {
        all.extend(cursor_activity(ctx));
    }
    if p.codex {
        all.extend(codex_activity(ctx));
    }
    if p.gemini {
        all.extend(antigravity_activity());
    }
    if p.workbuddy {
        all.extend(workbuddy_activity(ctx));
    }
    if p.dsh {
        all.extend(dsh_activity(ctx));
    }
    all
}

/// A path short enough to sit in one line of a report. The harness names every log
/// `session.v3.jsonl.zstd`, so the session's own directory is the only part that tells two of them
/// apart; the client transcripts are already named by their session id.
fn file_label(path: &std::path::Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if name.starts_with("session.") {
        if let Some(dir) = path.parent().and_then(|p| p.file_name()) {
            return dir.to_string_lossy().to_string();
        }
    }
    name
}

/// The raw material behind the Codex working-state decision.
fn codex_probe(now: u64) -> String {
    let Some(p) = crate::codex::newest_rollout() else { return "Codex activity: no rollout found".into() };
    let age = now.saturating_sub(mtime_ms(&p).unwrap_or(0)) / 1000;
    let step = crate::codex::tail_text(&p).and_then(|t| codex_last_step(&t));
    let tail: Vec<String> = crate::codex::tail_text(&p)
        .map(|t| {
            t.lines()
                .rev()
                .filter(|l| !l.trim().is_empty())
                .take(6)
                .map(|l| {
                    serde_json::from_str::<serde_json::Value>(l)
                        .map(|v| {
                            format!(
                                "{}/{}/{}",
                                v.get("type").and_then(|x| x.as_str()).unwrap_or("?"),
                                v.pointer("/payload/type").and_then(|x| x.as_str()).unwrap_or("-"),
                                v.pointer("/payload/role").and_then(|x| x.as_str()).unwrap_or("-")
                            )
                        })
                        .unwrap_or_else(|_| "(not a JSON line)".into())
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        "Codex activity: rollout={} modified {age}s ago | last step={:?} | last 6 lines (type/payload.type/role)=[{}]",
        p.display(),
        step.map(|(s, ts)| format!("{s:?} @{}s ago", now.saturating_sub(ts) / 1000)),
        tail.join(", ")
    )
}

/// The transcripts the WorkBuddy half is following and what each one's last entry said, so a wrong
/// answer can be traced to the file it came from instead of guessed at.
fn workbuddy_probe(now: u64) -> String {
    let files = crate::workbuddy_tokens::recent_transcripts(ACTIVE_MS);
    if files.is_empty() {
        return format!("WorkBuddy activity: no transcript written to in the last {}s", ACTIVE_MS / 1000);
    }
    let parts: Vec<String> = files
        .iter()
        .take(4)
        .map(|(path, _)| {
            let step = crate::workbuddy_tokens::read_state(path)
                .and_then(|s| s.step)
                .map(|(step, at)| format!("{step:?} @{}s ago", now.saturating_sub(at) / 1000))
                .unwrap_or_else(|| "no state entry".into());
            format!("{} -> {step}", file_label(path))
        })
        .collect();
    format!("WorkBuddy activity: {} in window [{}]", files.len(), parts.join(", "))
}

/// The harness sessions being followed and the last turn edge in each, which is the whole of that
/// half's decision.
fn dsh_probe(now: u64) -> String {
    let logs = crate::dsh::session_logs();
    if logs.is_empty() {
        return "DeepSeek Harness activity: no session logs".into();
    }
    let live: Vec<_> = logs.iter().filter(|p| now.saturating_sub(file_sig(p).1) <= ACTIVE_MS).collect();
    let parts: Vec<String> = live
        .iter()
        .take(4)
        .map(|path| match crate::dsh::read_state(path) {
            Some(state) => format!(
                "{} -> {:?}{} @{}s ago",
                file_label(path),
                state.edge,
                // The reason only rides along with a close, and it is the half of the answer that
                // decides between green and nothing, so a report that omitted it could not be
                // checked against the log.
                state.outcome.as_deref().map(|k| format!("/{k}")).unwrap_or_default(),
                now.saturating_sub(state.at) / 1000
            ),
            None => format!("{} -> unreadable", file_label(path)),
        })
        .collect();
    format!(
        "DeepSeek Harness activity: {} of {} logs live [{}]",
        live.len(),
        logs.len(),
        parts.join(", ")
    )
}

/// For doctor: the raw material behind every working-state decision. One line per provider, because
/// the thresholds and the evidence are different in each and a report that merged them could not be
/// checked against the files.
pub fn probe() -> String {
    let now = now_ms();
    [codex_probe(now), workbuddy_probe(now), dsh_probe(now)].join("\n  ")
}

#[cfg(windows)]
pub fn lower_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}
#[cfg(not(windows))]
pub fn lower_thread_priority() {}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        lower_thread_priority(); // the probe always yields to foreground input
        let mut ctx = Ctx::new();
        let mut last: Vec<Activity> = Vec::new();
        let mut pres = presence();
        let mut tick: u32 = 0;
        loop {
            // Presence checks (finding the exe, reading credentials) once a minute are plenty; the 2 s tick does only stats and a query
            if tick.is_multiple_of(30) {
                pres = presence();
            }
            tick = tick.wrapping_add(1);
            let found = read_all(pres, &mut ctx);
            if found != last {
                // Log the first 20 state changes (with the Codex raw material) so thresholds can be calibrated
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                    crate::applog(&format!("activity: {:?} | {}", found.iter().map(|a| format!("{}:{}", a.provider, a.state)).collect::<Vec<_>>(), probe()));
                }
                last = found.clone();
                {
                    let st = app.state::<AppState>();
                    *st.activity.lock().unwrap() = found.clone();
                }
                let _ = app.emit("activity", &found);
            }
            std::thread::sleep(INTERVAL);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbuddy_tokens::Step;

    #[test]
    fn a_session_is_named_by_its_own_title_when_it_has_one() {
        assert_eq!(session_name("Fix the ring", "D:\\work\\thing", "WorkBuddy"), "Fix the ring");
        assert_eq!(session_name("  让项目1支持WorkBuddy  ", "", "WorkBuddy"), "让项目1支持WorkBuddy");
    }

    /// The directory is the fallback that still says which session it is, and the provider name is
    /// the fallback for the fallback — a blank row would be worse than either.
    #[test]
    fn a_session_without_a_title_is_named_by_the_directory_it_runs_in() {
        assert_eq!(session_name("", "D:\\WorkBuddyWorkSpace\\2026-09-27-00-18-14", "WorkBuddy"),
                   "2026-09-27-00-18-14");
        assert_eq!(session_name("", "/home/dav/vault/", "DeepSeek Harness"), "vault");
        assert_eq!(session_name("", "D:\\OB\\DAV", "DeepSeek Harness"), "DAV");
        assert_eq!(session_name("", "", "WorkBuddy"), "WorkBuddy");
        // A drive root has no last segment worth printing, and "C:" names nothing.
        assert_eq!(session_name("", "C:\\", "WorkBuddy"), "WorkBuddy");
        assert_eq!(session_name("", "/", "DeepSeek Harness"), "DeepSeek Harness");
    }

    /// The boundaries are the whole content of the decision, so they are asserted on both sides of
    /// each one. A window that is a millisecond too tight looks exactly like a correct one until
    /// the day it hides work that was happening.
    #[test]
    fn each_kind_of_step_gets_the_window_it_deserves() {
        assert!(step_still_running(Step::Tool, 0));
        assert!(step_still_running(Step::Tool, TOOL_QUIET_MS));
        assert!(!step_still_running(Step::Tool, TOOL_QUIET_MS + 1));

        assert!(step_still_running(Step::Thinking, THINKING_QUIET_MS));
        assert!(!step_still_running(Step::Thinking, THINKING_QUIET_MS + 1));
        assert!(step_still_running(Step::Asked, THINKING_QUIET_MS));
        assert!(!step_still_running(Step::Asked, THINKING_QUIET_MS + 1));

        assert!(step_still_running(Step::Answered, ANSWER_QUIET_MS));
        assert!(!step_still_running(Step::Answered, ANSWER_QUIET_MS + 1));

        // A tool call outlives a thought by an order of magnitude, which is the point of having
        // three windows: a build is not a thought and must not be given a thought's allowance.
        assert!(TOOL_QUIET_MS > THINKING_QUIET_MS * 4);
        assert!(THINKING_QUIET_MS > ANSWER_QUIET_MS * 4);
        // The listing filter is the widest of them, or a file could be dropped while a step in it
        // was still inside its own window.
        assert!(ACTIVE_MS >= TOOL_QUIET_MS);
    }

    // ---- the fifth state: a run that has just finished ----

    /// A session log with the state under test. `at` is when the last edge was written, which is
    /// what the green window counts from.
    fn logged(edge: crate::dsh::Edge, outcome: Option<&str>, at: u64) -> crate::dsh::State {
        crate::dsh::State {
            edge: Some(edge),
            at,
            outcome: outcome.map(str::to_string),
            ..Default::default()
        }
    }

    /// The green ring is an announcement, so like every announcement it has to end by itself. Ten
    /// seconds is upstream's own number, and it is asserted on both sides of the boundary.
    #[test]
    fn a_completed_turn_is_drawn_green_and_then_stops() {
        let finished = logged(crate::dsh::Edge::TurnClosed, Some("completed"), 1_000);
        assert_eq!(dsh_showing(&finished, 0, 1_000), Some(("success", "Complete")));
        assert_eq!(dsh_showing(&finished, 0, 1_000 + SUCCESS_MS), Some(("success", "Complete")));
        assert_eq!(dsh_showing(&finished, 0, 1_001 + SUCCESS_MS), None, "the window has to close");
    }

    /// `turn/end` closes a turn whatever ended it, and four of the five reasons are not a piece of
    /// work finishing. A green ring on an aborted or failed turn would say the opposite of what
    /// happened, so only the harness's own `completed` draws one.
    #[test]
    fn only_a_completed_turn_is_ever_drawn_green() {
        for outcome in [None, Some("aborted"), Some("error"), Some("max-tokens"), Some("blocked")] {
            let closed = logged(crate::dsh::Edge::TurnClosed, outcome, 1_000);
            assert_eq!(dsh_showing(&closed, 0, 1_000), None, "{outcome:?} is not a completion");
        }
    }

    /// Waiting outranks working because it is the one state that wants something from you, and it is
    /// the only one that never expires — a question stays on screen until it is answered.
    #[test]
    fn a_question_outlasts_every_window() {
        let asking = logged(crate::dsh::Edge::ApprovalAsked, None, 1_000);
        assert_eq!(dsh_showing(&asking, 0, 1_000), Some(("waiting", "needs your input")));
        assert_eq!(
            dsh_showing(&asking, 10 * ACTIVE_MS, 1_000),
            Some(("waiting", "needs your input")),
            "an unanswered question does not go away because time passed"
        );
    }

    /// A turn left open is believed while the *file* is still moving, not for as long as the turn is
    /// old: a turn that started twenty minutes ago is still running if the log is being appended to.
    /// The two clocks are the reason this takes the file's age as well as the clock.
    #[test]
    fn an_open_turn_is_believed_while_the_log_keeps_moving() {
        let open = logged(crate::dsh::Edge::TurnOpen, None, 1_000);
        assert_eq!(dsh_showing(&open, 0, 20 * ACTIVE_MS), Some(("busy", "Working")));
        assert_eq!(dsh_showing(&open, ACTIVE_MS, 20 * ACTIVE_MS), Some(("busy", "Working")));
        assert_eq!(dsh_showing(&open, ACTIVE_MS + 1, 20 * ACTIVE_MS), None, "killed, not thinking");
    }

    /// A session that answered and was then answered back to is working again, not finished: the
    /// approval that settled it put the turn back in flight.
    #[test]
    fn an_answered_approval_puts_the_session_back_to_work() {
        let settled = logged(crate::dsh::Edge::ApprovalSettled, None, 1_000);
        assert_eq!(dsh_showing(&settled, 0, 1_000), Some(("busy", "Working")));
    }

    /// A log that has not started a turn says nothing at all, which is not the same as saying it is
    /// idle — the card draws no row for it rather than an empty one.
    #[test]
    fn a_log_with_no_edges_at_all_is_not_drawn() {
        let empty = crate::dsh::State::default();
        assert_eq!(dsh_showing(&empty, 0, 0), None);
        assert!(!empty.working(), "not knowing a state is not the same as knowing it is idle");
        assert!(!empty.waiting());
        assert!(!empty.completed(), "and an absent turn is not a finished one either");
    }

    /// Every state has a length it is believed for, and the announcement is the shortest of them:
    /// it has to expire without anything writing to a file to say so.
    #[test]
    fn the_announcement_is_the_shortest_thing_on_screen() {
        assert_eq!(window_of("success"), SUCCESS_MS);
        assert_eq!(window_of("busy"), ACTIVE_MS);
        assert_eq!(window_of("waiting"), ACTIVE_MS);
        assert!(SUCCESS_MS < ACTIVE_MS);
    }

    /// What a provider's rows are sorted by. Waiting first — it is the only one asking for
    /// something — and a run that just finished ahead of one still going, because catching your eye
    /// is the whole point of it.
    #[test]
    fn the_rows_a_provider_draws_are_ordered_waiting_then_working_then_finished() {
        assert!(rank("waiting") < rank("busy"));
        assert!(rank("busy") < rank("success"));
        assert!(rank("success") < rank("something-a-plugin-invented"));
    }

    // ---- Codex: the rollout's own completion marker ----

    /// Codex states the end of a turn outright with `task_complete`, and states an interrupted one
    /// with `turn_aborted` right beside it. Only the first is a piece of work finishing — this is
    /// the distinction upstream's rollout reader draws, and drawing a green ring for the second
    /// would report an abort as a success.
    #[test]
    fn codex_draws_green_for_a_completed_turn_and_nothing_for_an_aborted_one() {
        assert_eq!(codex_showing(CodexStep::Complete, 0), Some("success"));
        assert_eq!(codex_showing(CodexStep::Complete, SUCCESS_MS), Some("success"));
        assert_eq!(codex_showing(CodexStep::Complete, SUCCESS_MS + 1), None);

        assert_eq!(codex_showing(CodexStep::Aborted, 0), None);
        assert_eq!(codex_showing(CodexStep::Aborted, SUCCESS_MS), None);
    }

    /// The three working states keep the windows they had, asserted either side of each boundary.
    #[test]
    fn the_codex_working_windows_are_unchanged() {
        assert_eq!(codex_showing(CodexStep::Tool, TOOL_QUIET_MS), Some("busy"));
        assert_eq!(codex_showing(CodexStep::Tool, TOOL_QUIET_MS + 1), None);
        assert_eq!(codex_showing(CodexStep::Thinking, THINKING_QUIET_MS), Some("busy"));
        assert_eq!(codex_showing(CodexStep::Thinking, THINKING_QUIET_MS + 1), None);
        assert_eq!(codex_showing(CodexStep::AsstMsg, ANSWER_QUIET_MS), Some("busy"));
        assert_eq!(codex_showing(CodexStep::AsstMsg, ANSWER_QUIET_MS + 1), None);
    }

    /// A step is dated from the record, not from the write. The two cases that pull the answer in
    /// opposite directions: a record that has a timestamp wins even when the file was written later
    /// (so a green ring appears when the work ended rather than when Codex stopped tidying up), and
    /// a record with no usable timestamp falls back to the file rather than to zero — a step dated
    /// at the epoch would be drawn as either stale for ever or, worse, as happening now.
    #[test]
    fn a_step_is_dated_from_its_record_and_only_falls_back_to_the_file() {
        assert_eq!(step_at(1_700_000_100_000, 1_700_000_000_000), 1_700_000_100_000);
        assert_eq!(step_at(1_700_000_100_000, 1_700_000_176_000), 1_700_000_100_000,
                   "76 s of bookkeeping after the fact must not date the step");
        assert_eq!(step_at(0, 1_700_000_000_000), 1_700_000_000_000);
    }
}
