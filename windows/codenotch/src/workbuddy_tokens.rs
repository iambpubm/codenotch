//! WorkBuddy's own session transcripts, totalled as tokens per local day.
//!
//! This is the half of the WorkBuddy cell that keeps working when the other half cannot. The quota
//! endpoint in `workbuddy.rs` needs the app's session credential, and WorkBuddy 5.6.0 and later seal
//! each credential field with a key only its own runtime holds — on a machine like that there is no
//! balance to read at all. The transcripts, by contrast, are plain JSONL on disk and carry every
//! call's token counts.
//!
//! Token Monitor reads the same tree, through `tokscale` (`src/shared/clientSources.js`); this is
//! that data plane read directly rather than through its scanner.
//!
//! Where the data lives — one JSONL file per session, under whichever root the installed version
//! writes to:
//!
//! ```text
//! ~/.workbuddy/projects/<encoded-project>/<session-id>.jsonl      through 5.4
//! ~/.workbuddy-ai/projects/<encoded-project>/<session-id>.jsonl   5.5 and later
//! ```
//!
//! Both roots are read, and on an upgraded machine neither is a superset of the other: the older
//! tree keeps the history written before the move. Token Monitor keeps both for the same reason.
//!
//! A transcript is a stream of `{type, timestamp, …}` records — `message`, `reasoning`,
//! `function_call`, `function_call_result`, `file-history-snapshot`, `session-meta`, `ai-title`.
//! Only `function_call` carries accounting, as an embedded `message.usage`:
//!
//! ```text
//! { "input_tokens": 40734, "output_tokens": 309, "total_tokens": 41043,
//!   "cache_read_input_tokens": 9856 }
//! ```
//!
//! The rules that have to survive a refactor:
//!
//!   - **`input_tokens` is the whole context every call re-sends, not this call's addition.**
//!     Summing `total_tokens` would charge the same history once per call, inflating a long session
//!     by two orders of magnitude. What is summed instead is
//!     `(input_tokens - min(cache_read_input_tokens, input_tokens)) + output_tokens` — the tokens
//!     the provider actually had to process — which is the only figure that stays comparable
//!     between a two-call session and a nine-hundred-call one.
//!   - **A torn tail is normal, not an error.** The app appends while a scan runs, so the last line
//!     can be half-written. A line that will not parse is skipped; it is never a reason to refuse
//!     the file.
//!   - **A file's records are never newer than the file.** Records are appended, so a file whose
//!     last write predates the kept window cannot hold a record inside it. That makes the mtime
//!     filter exact rather than approximate, and it is what keeps a scan off the whole history.
//!   - **A file already totalled is not folded again.** Each file's per-day totals are cached
//!     against its size and mtime, so a poll where nothing changed costs one `metadata` per file.
//!   - **Two spellings of one root resolve once.** `~/.workbuddy` is a link on a machine that moved
//!     it, so the roots are canonicalised before the scan rather than after the totals.

use crate::usage::LimitWindow;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Roots under the user's home directory. `~/.workbuddy` is what through-5.4 wrote; 5.5 moved to
/// `~/.workbuddy-ai` and left the older tree in place beside it.
const ROOT_NAMES: [&str; 2] = [".workbuddy", ".workbuddy-ai"];
const PROJECTS_DIR: &str = "projects";
const TRANSCRIPT_EXT: &str = ".jsonl";
/// Days kept. Past this nothing is reported, so nothing needs keeping — and a file untouched since
/// before the window is skipped without ever being opened.
const KEEP_DAYS: i32 = 31;
/// `<projects>/<encoded-project>/<session>.jsonl`: one level of directories below the root.
const MAX_PROJECT_DEPTH: usize = 1;
/// Bounds on a runaway tree, so a scan can never become a walk of the whole disk.
const MAX_DIRS: usize = 20_000;
/// One session's transcript. Larger than this is not one conversation; it is refused, not read.
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn mtime_ms_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------- where the transcripts live ----------------

/// `~/.workbuddy/projects` and `~/.workbuddy-ai/projects`, deduplicated by resolved path so a linked
/// root is scanned once however it is spelled.
fn roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for name in ROOT_NAMES {
        let dir = home.join(name).join(PROJECTS_DIR);
        let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(dir);
    }
    out
}

/// Whether there is a transcript tree at all, which is what decides whether this half of the cell
/// has anything to say.
pub fn present() -> bool {
    roots().iter().any(|p| p.is_dir())
}

/// Every `*.jsonl` under the roots, at most `MAX_PROJECT_DEPTH` levels down.
fn discover(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0)).collect();
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > MAX_DIRS {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if depth < MAX_PROJECT_DEPTH {
                    stack.push((entry.path(), depth + 1));
                }
            } else if kind.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(TRANSCRIPT_EXT) {
                    files.push(entry.path());
                }
            }
        }
    }
    files
}

// ---------------- folding one transcript ----------------

/// Local calendar day of an instant, as days since the Common Era — an integer that compares and
/// subtracts without any daylight-saving arithmetic.
fn local_day(ms: u64) -> i32 {
    use chrono::{Datelike, Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|d| d.date_naive().num_days_from_ce())
        .unwrap_or(0)
}

fn u64_of(value: Option<&serde_json::Value>) -> u64 {
    match value {
        Some(serde_json::Value::Number(n)) => {
            n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0)
        }
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

/// What one call cost, as the provider states it. `input_tokens` is the whole context the call
/// re-sent, so only the part the cache did not serve is new work; the output is added on top. A
/// cache figure larger than the input is clamped rather than subtracted below zero.
fn tokens_of(usage: &serde_json::Value) -> u64 {
    let input = u64_of(usage.get("input_tokens"));
    let cached = u64_of(usage.get("cache_read_input_tokens")).min(input);
    input.saturating_sub(cached).saturating_add(u64_of(usage.get("output_tokens")))
}

/// Tokens per local day for one transcript.
fn fold_file(path: &Path) -> BTreeMap<i32, u64> {
    let Ok(bytes) = std::fs::read(path) else { return BTreeMap::new() };
    let text = String::from_utf8_lossy(&bytes);
    let mut days: BTreeMap<i32, u64> = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        // Cheap rejection ahead of the parser: most records in a transcript carry no accounting at
        // all, and `usage` is the only key that leads to one.
        if trimmed.is_empty() || !trimmed.contains("\"usage\"") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
        let time = u64_of(record.get("timestamp"));
        if time == 0 {
            continue;
        }
        let Some(usage) = record.get("message").and_then(|m| m.get("usage")) else { continue };
        if !usage.is_object() {
            continue;
        }
        let tokens = tokens_of(usage);
        if tokens == 0 {
            continue;
        }
        *days.entry(local_day(time)).or_insert(0) += tokens;
    }
    days
}

// ---------------- the per-file cache ----------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileTotals {
    size: u64,
    mtime_ms: u64,
    /// Tokens per local day (days-from-CE), so a restart does not have to fold the whole history
    /// again.
    days: BTreeMap<i32, u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TotalsCache {
    files: HashMap<String, FileTotals>,
}

/// One pass: fold what changed, reuse what did not, and total every live transcript by day. The
/// returned count is the number of transcripts that contributed.
pub fn scan(cache: &mut TotalsCache, now: u64) -> (BTreeMap<i32, u64>, usize) {
    let cutoff_ms = now.saturating_sub(KEEP_DAYS as u64 * DAY_MS);
    let mut live: HashMap<String, FileTotals> = HashMap::new();
    let mut total: BTreeMap<i32, u64> = BTreeMap::new();

    for path in &discover(&roots()) {
        let Ok(meta) = std::fs::metadata(path) else { continue };
        let size = meta.len();
        if size > MAX_FILE_BYTES {
            continue;
        }
        let mtime_ms = mtime_ms_of(&meta);
        if mtime_ms < cutoff_ms {
            continue;
        }
        let key = path.to_string_lossy().to_string();
        let cached = cache
            .files
            .get(&key)
            .filter(|entry| entry.size == size && entry.mtime_ms == mtime_ms)
            .cloned();
        let entry = match cached {
            Some(entry) => entry,
            None => FileTotals { size, mtime_ms, days: fold_file(path) },
        };
        for (day, tokens) in &entry.days {
            *total.entry(*day).or_insert(0) += *tokens;
        }
        live.insert(key, entry);
    }
    let counted = live.len();
    cache.files = live;
    (total, counted)
}

fn total_between(days: &BTreeMap<i32, u64>, from_day: i32, to_day: i32) -> u64 {
    days.iter().filter(|(day, _)| **day >= from_day && **day <= to_day).map(|(_, t)| *t).sum()
}

fn count_window(id: &str, label: &str, tokens: u64) -> LimitWindow {
    LimitWindow {
        id: id.into(),
        label: label.into(),
        used: 0.0,
        resets_at: None,
        count: Some(tokens as i64),
        unit: Some("tokens".into()),
        ..Default::default()
    }
}

/// Today, then the wider windows once they hold something. These are count windows: WorkBuddy
/// publishes no allowance covering local usage, and a percentage of an unpublished limit is a guess.
/// "Today" is always present so the cell always means the same thing.
pub fn windows(days: &BTreeMap<i32, u64>, now: u64) -> Vec<LimitWindow> {
    let today = local_day(now);
    let mut out = vec![count_window("today", "Today", total_between(days, today, today))];
    for (id, label, span) in [("week", "Last 7 days", 6), ("month", "Last 30 days", 29)] {
        let tokens = total_between(days, today - span, today);
        if tokens > 0 {
            out.push(count_window(id, label, tokens));
        }
    }
    out
}

/// For doctor: what this half can see, naming no path beyond the root names and no conversation.
pub fn probe() -> String {
    let roots = roots();
    if roots.is_empty() {
        return "WorkBuddy transcripts: no home directory to look under".into();
    }
    if !present() {
        return format!(
            "WorkBuddy transcripts: no {PROJECTS_DIR} directory under {} or {}",
            ROOT_NAMES[0], ROOT_NAMES[1]
        );
    }
    let now = now_ms();
    let mut cache = TotalsCache::default();
    let (days, files) = scan(&mut cache, now);
    let today = local_day(now);
    format!(
        "WorkBuddy transcripts: {files} file(s) written in the last {KEEP_DAYS} days, {} tokens today, {} in 30 days",
        total_between(&days, today, today),
        total_between(&days, today - 29, today)
    )
}

/// For the diagnostic report: how much history there is, as counts and sizes only — the project
/// directories are encoded working paths, which can name a customer or a project, so none of them is
/// ever printed. Returns `(project directories, transcripts, total bytes, newest write in ms)`.
pub fn survey() -> (usize, usize, u64, u64) {
    let mut projects = 0usize;
    let mut logs = 0usize;
    let mut bytes = 0u64;
    let mut newest = 0u64;
    for root in roots() {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            projects += 1;
            let Ok(inner) = std::fs::read_dir(entry.path()) else { continue };
            for file in inner.flatten() {
                if !file.file_name().to_string_lossy().ends_with(TRANSCRIPT_EXT) {
                    continue;
                }
                logs += 1;
                if let Ok(meta) = file.metadata() {
                    bytes += meta.len();
                    newest = newest.max(mtime_ms_of(&meta));
                }
            }
        }
    }
    (projects, logs, bytes, newest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_cached_part_of_the_context_is_not_charged_again() {
        let usage = json!({
            "input_tokens": 40734,
            "output_tokens": 309,
            "cache_read_input_tokens": 9856
        });
        assert_eq!(tokens_of(&usage), 40734 - 9856 + 309);
    }

    #[test]
    fn a_fully_cached_call_costs_only_its_output() {
        let usage = json!({
            "input_tokens": 41186,
            "output_tokens": 238,
            "cache_read_input_tokens": 40960
        });
        assert_eq!(tokens_of(&usage), 226 + 238);
    }

    #[test]
    fn a_cache_figure_larger_than_the_input_clamps_at_the_output() {
        let usage = json!({ "input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 999 });
        assert_eq!(tokens_of(&usage), 1);
    }

    #[test]
    fn a_call_with_no_cache_at_all_is_input_plus_output() {
        let usage = json!({ "input_tokens": 120, "output_tokens": 30 });
        assert_eq!(tokens_of(&usage), 150);
    }

    #[test]
    fn a_usage_object_without_numbers_costs_nothing() {
        assert_eq!(tokens_of(&json!({ "input_tokens": "nonsense" })), 0);
        assert_eq!(tokens_of(&json!({})), 0);
    }

    #[test]
    fn windows_total_the_days_they_name() {
        let now = now_ms();
        let today = local_day(now);
        let mut days: BTreeMap<i32, u64> = BTreeMap::new();
        days.insert(today, 10);
        days.insert(today - 1, 20);
        days.insert(today - 6, 30);
        days.insert(today - 7, 40); // outside the seven-day window
        days.insert(today - 29, 50);
        days.insert(today - 30, 60); // outside the thirty-day window

        let w = windows(&days, now);
        let by_id = |id: &str| w.iter().find(|x| x.id == id).cloned();
        assert_eq!(by_id("today").unwrap().count, Some(10));
        assert_eq!(by_id("week").unwrap().count, Some(60));
        assert_eq!(by_id("month").unwrap().count, Some(110));
        assert_eq!(by_id("today").unwrap().unit.as_deref(), Some("tokens"));
        assert_eq!(by_id("today").unwrap().used, 0.0);
    }

    #[test]
    fn a_quiet_day_still_reports_today_and_hides_the_wider_windows() {
        let now = now_ms();
        let today = local_day(now);
        let days: BTreeMap<i32, u64> = BTreeMap::new();
        let w = windows(&days, now);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].id, "today");
        assert_eq!(w[0].count, Some(0));
        assert_eq!(total_between(&days, today - 29, today), 0);
    }

    #[test]
    fn a_transcript_is_folded_by_day_and_a_torn_tail_is_skipped() {
        let dir = std::env::temp_dir().join(format!("codenotch-wb-tokens-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("session.jsonl");
        let at = 1_700_000_000_000u64;
        let lines = [
            // Not a call: no accounting at all, and skipped before the parser sees it.
            r#"{"type":"message","timestamp":1700000000000,"role":"user","content":"hi"}"#,
            // A call: 100 in, 50 of it served from cache, 10 out → 60.
            r#"{"type":"function_call","timestamp":1700000000000,"message":{"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":50,"total_tokens":110}}}"#,
            // The app appends while a scan runs, so the last line is routinely half-written.
            r#"{"type":"function_call","timestamp":1700000000001,"mess"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let days = fold_file(&path);
        assert_eq!(days.get(&local_day(at)), Some(&60));
        assert_eq!(days.len(), 1, "one session's calls land on one day here");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
