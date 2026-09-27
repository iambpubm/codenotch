//! DeepSeek Harness (`dsh`) usage adapter, ported from Token Monitor's dsh provider
//! (`src/shared/providers/dsh/paths.js`, `sessionFiles.js`, `sessionDetail.js`) and from the
//! provider notes in `docs/providers/dsh.md`.
//!
//! DeepSeek Harness publishes no quota and has no balance endpoint, so this cell is not a
//! percentage of an allowance — it is how much you have actually spent, read from the harness's own
//! transcripts and totalled for today, the last seven days and the last thirty. That is the same
//! bargain the other providers strike (borrow the client's own local state, read only), and it is
//! also why the windows are count windows: a limit nobody published cannot be drawn as a ring.
//!
//! Where the data lives. The harness resolves its home from `%DSH_HOME%`, falling back to
//! `~/.dsh`, and writes one transcript per session:
//!
//! ```text
//! <dshHome>/sessions/<encoded-cwd>/<session-id>/session[.<version>].jsonl[.zstd]
//! ```
//!
//! A v3-and-later harness re-encodes a session into a *new* versioned file and leaves the old one
//! in place instead of rotating it, so one session directory can hold both encodings at once and
//! the versioned file is the live one. Only the highest-ranked transcript in a directory is read:
//! the versioned file is a complete re-encode, and reading both would charge every token twice.
//!
//! Records are `{type, seq, time, data}` envelopes. Token usage lives in
//! `data.usage.{inputTokens,outputTokens,cacheReadTokens,cacheWriteTokens,reasoningTokens}` — or,
//! for a call that never produced a surface message, in the last `data.stream[].chunk.usage`. The
//! three event kinds that carry it are `assistant/message`, `assistant/attempt` and
//! `compaction/summary`.
//!
//! The rules that have to survive a refactor:
//!
//!   - **`outputTokens` is passed through unmodified.** DSH's reasoning is a *subset* of output, so
//!     subtracting it here would under-count every reasoning-heavy session by exactly its reasoning
//!     tokens. The total is input + output + cacheRead + cacheWrite.
//!   - **A replayed line is not a second charge.** dsh's writer can re-append an already-flushed
//!     record, so records are deduped on message identity + time + routing + token signature.
//!   - **A forked session must not be charged for its parent.** Its log opens with a byte-for-byte
//!     copy of the parent's events, and the log says where the copy ends. A legacy header states the
//!     cut as `seedLength`; a current header carries `isSeeded: true` and puts it on the *last*
//!     `session/end-seed` marker whose data says `inherited: true`. Either way the cut is a seq
//!     number: the first event the child itself owns. Every event strictly below it is the parent's
//!     and is dropped; the event *on* it is charged.
//!   - **Silence and denial are different answers.** A seeded log that carries no `end-seed` marker
//!     at all cannot say where its copy ended, so the whole session is charged nothing rather than
//!     billing the parent's prefix to the child. An `end-seed` that is present but not tagged
//!     `inherited` has declared outright that nothing was inherited, so its history stands.
//!   - **A torn tail is normal, not an error.** The harness appends one Zstandard frame per flush,
//!     so a transcript scanned mid-write routinely ends in a half-written frame. Frame boundaries
//!     are located without decompressing (`scan_zstd_frames`), so the complete frames are kept and
//!     only the fragment at the cut is dropped.
//!   - **The first frame that will not decode is the recovery boundary.** Nothing after it is
//!     trusted, because a corrupt frame means the file is not what we think it is.

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const POLL_SECS: u64 = 300;
/// The harness home is looked for again this often while it is not there at all.
const ABSENT_POLL_SECS: u64 = 600;
/// `<root>/<project>/<session>/<artifact>`: two levels of directories below `sessions/`.
const MAX_SESSION_DIR_DEPTH: usize = 2;
/// Bounds on a hostile or runaway tree, so a scan can never turn into a disk walk of the machine.
const MAX_DIRS: usize = 20_000;
/// A transcript larger than this is not one session's conversation; it is refused rather than read.
const MAX_TRANSCRIPT_BYTES: u64 = 256 * 1024 * 1024;
const ZSTD_MAGIC: u32 = 0xFD2B_2F28;
/// Days kept in the per-file cache: past this nothing is reported, so nothing needs keeping.
const KEEP_DAYS: i32 = 31;

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("dsh.json")
}

fn cache_path() -> PathBuf {
    crate::config::config_path().with_file_name("dsh-cache.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

// ---------------- where the harness keeps its sessions ----------------

/// `%DSH_HOME%` when set, otherwise `~/.dsh` — the same resolution the harness itself uses, so a
/// relocated home is picked up without any setting of ours.
pub fn home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("DSH_HOME") {
        let text = v.to_string_lossy().trim().to_string();
        if !text.is_empty() {
            return Some(PathBuf::from(text));
        }
    }
    dirs::home_dir().map(|h| h.join(".dsh"))
}

pub fn sessions_root() -> Option<PathBuf> {
    home().map(|h| h.join("sessions"))
}

pub fn present() -> bool {
    sessions_root().map(|p| p.is_dir()).unwrap_or(false)
}

// ---------------- transcript discovery ----------------

/// The harness's own naming: `session.jsonl`, `session.jsonl.zstd`, `session.v3.jsonl`,
/// `session.v3.jsonl.zstd`. The version segment is matched generically (numeric, `v`-prefixed) so a
/// future generation is still found, and deliberately not as `session.<anything>` — an unrelated
/// file dropped into a session directory must not be mistaken for a transcript.
fn session_log_rank(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("session.")?;
    let (version, ext) = match rest.strip_prefix('v') {
        Some(after_v) => {
            let digits: String = after_v.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            let tail = after_v.get(digits.len()..)?;
            let tail = tail.strip_prefix('.')?;
            (digits.parse::<u32>().ok()?, tail)
        }
        None => (0u32, rest),
    };
    if ext == "jsonl" || ext == "jsonl.zstd" {
        Some(version)
    } else {
        None
    }
}

/// One path per session directory: the live transcript, chosen by version rank and then by name so
/// the choice never depends on the order the filesystem happens to return.
fn preferred_transcripts(root: &Path) -> Vec<PathBuf> {
    let mut best: BTreeMap<PathBuf, (u32, String)> = BTreeMap::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
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
                if depth < MAX_SESSION_DIR_DEPTH {
                    stack.push((entry.path(), depth + 1));
                }
            } else if kind.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(rank) = session_log_rank(&name) else { continue };
                let replace = best
                    .get(&dir)
                    .map(|(current, current_name)| (rank, &name) > (*current, current_name))
                    .unwrap_or(true);
                if replace {
                    best.insert(dir.clone(), (rank, name));
                }
            }
        }
    }
    best.into_iter().map(|(dir, (_, name))| dir.join(name)).collect()
}

// ---------------- Zstandard frames ----------------

/// Frame boundaries located **without decompressing**, so a torn trailing frame — the normal state
/// of a transcript being written right now — is skipped instead of aborting the parse. Ported from
/// dsh's own session-persistence-jsonl backend (MIT) by way of Token Monitor.
fn scan_zstd_frames(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset < buf.len() {
        let start = offset;
        if buf.len() - offset < 4 {
            return frames;
        }
        if u32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]]) != ZSTD_MAGIC {
            return frames;
        }
        offset += 4;
        if offset == buf.len() {
            return frames;
        }
        let descriptor = buf[offset];
        offset += 1;
        if descriptor & 0x18 != 0 {
            return frames; // reserved bits set — a torn or corrupt tail
        }
        let content_size_flag = descriptor >> 6;
        let single_segment = descriptor & 0x20 != 0;
        let checksum = descriptor & 0x04 != 0;
        let dictionary_flag = descriptor & 0x03;
        let dictionary_bytes = if dictionary_flag == 3 { 4usize } else { dictionary_flag as usize };
        let content_size_bytes = if content_size_flag == 0 {
            if single_segment { 1 } else { 0 }
        } else {
            1usize << content_size_flag
        };
        let remaining_header = (if single_segment { 0 } else { 1 }) + dictionary_bytes + content_size_bytes;
        if buf.len() - offset < remaining_header {
            return frames;
        }
        offset += remaining_header;
        loop {
            if buf.len() - offset < 3 {
                return frames;
            }
            let block_header =
                (buf[offset] as u32) | ((buf[offset + 1] as u32) << 8) | ((buf[offset + 2] as u32) << 16);
            offset += 3;
            let last_block = block_header & 1 != 0;
            let block_type = (block_header >> 1) & 0x03;
            let block_size = (block_header >> 3) as usize;
            if block_type == 0x03 {
                return frames; // reserved block type — a torn or corrupt tail
            }
            let payload = if block_type == 0x01 { 1usize } else { block_size };
            if buf.len() - offset < payload {
                return frames;
            }
            offset += payload;
            if last_block {
                break;
            }
        }
        if checksum {
            if buf.len() - offset < 4 {
                return frames;
            }
            offset += 4;
        }
        frames.push((start, offset));
    }
    frames
}

/// Decode one frame's worth of bytes. On a half-written frame the decoder reports EOF partway
/// through; `read_to_end` keeps every byte it managed to produce before that, which is exactly the
/// records the harness had finished writing.
fn decode_frame_lossy(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::new();
    if let Ok(mut decoder) = zstd::stream::read::Decoder::new(std::io::Cursor::new(bytes)) {
        let _ = decoder.read_to_end(&mut out);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The whole transcript as text. A frame whose framing is complete but whose content is corrupt
/// stops the decode: keeping records past it would resurrect history the harness itself would not
/// read back.
fn decode_transcript(bytes: &[u8], compressed: bool) -> String {
    if !compressed {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let frames = scan_zstd_frames(bytes);
    let mut out = String::new();
    let mut consumed = 0usize;
    for (start, end) in &frames {
        let mut buf: Vec<u8> = Vec::new();
        let decoded = zstd::stream::read::Decoder::new(std::io::Cursor::new(&bytes[*start..*end]))
            .map(|mut d| d.read_to_end(&mut buf).is_ok())
            .unwrap_or(false);
        if !decoded {
            return out;
        }
        out.push_str(&String::from_utf8_lossy(&buf));
        consumed = *end;
    }
    if consumed < bytes.len() {
        let tail = &bytes[consumed..];
        if tail.len() >= 4 && u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) == ZSTD_MAGIC {
            out.push_str(&decode_frame_lossy(tail));
        }
    }
    out
}

fn read_transcript(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_TRANSCRIPT_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let compressed = path.to_string_lossy().ends_with(".jsonl.zstd");
    Some(decode_transcript(&bytes, compressed))
}

// ---------------- parsing one transcript ----------------

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

fn i64_of(value: Option<&serde_json::Value>) -> i64 {
    match value {
        Some(serde_json::Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn str_of(value: Option<&serde_json::Value>) -> String {
    value.and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

/// Token counts as the harness states them. `output` is reasoning-inclusive and stays that way.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Tokens {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
}

impl Tokens {
    fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

fn tokens_of(usage: &serde_json::Value) -> Tokens {
    let get = |key: &str| i64_of(usage.get(key)).max(0);
    Tokens {
        input: get("inputTokens"),
        output: get("outputTokens"),
        cache_read: get("cacheReadTokens"),
        cache_write: get("cacheWriteTokens"),
        reasoning: get("reasoningTokens"),
    }
}

/// A call that never produced a surface message keeps its usage inside the embedded stream.
fn last_stream_usage(stream: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    let items = stream?.as_array()?;
    for item in items.iter().rev() {
        let usage = item.get("chunk").and_then(|c| c.get("usage"));
        if usage.map(|u| u.is_object()).unwrap_or(false) {
            return usage;
        }
    }
    None
}

#[derive(Debug, Clone)]
struct UsageEvent {
    record_index: usize,
    seq: Option<i64>,
    time: u64,
    provider: String,
    model: String,
    identity: String,
    tokens: Tokens,
    kind: &'static str,
}

/// Totals per local day for one transcript. Only complete, non-inherited, de-duplicated charges
/// are in here; everything the harness itself would not count is filtered out before it lands.
fn fold_transcript(text: &str) -> BTreeMap<i32, u64> {
    let mut header_id = String::new();
    let mut header_index: Option<usize> = None;
    let mut seed_length: Option<i64> = None;
    let mut is_seeded = false;
    let mut tagged_cut: Option<i64> = None;
    let mut seed_boundary_seen = false;
    let mut events: Vec<UsageEvent> = Vec::new();

    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
        let kind = record.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let data = record.get("data");
        match kind {
            "session" => {
                if header_index.is_none() {
                    header_index = Some(index);
                    header_id = str_of(record.get("id"));
                    seed_length = record.get("seedLength").and_then(|v| v.as_i64());
                    is_seeded = record.get("isSeeded").and_then(|v| v.as_bool()).unwrap_or(false);
                }
            }
            "session/end-seed" => {
                // Present at all, whether or not it is tagged, is information: it means this log
                // states a seeding outcome, so a `false` here outranks the header's lineage bit.
                seed_boundary_seen = true;
                let inherited = data
                    .and_then(|d| d.get("inherited"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if inherited {
                    // A fork can inherit an ancestor's tagged marker, so only the last one counts.
                    if let Some(seq) = record.get("seq").and_then(|s| s.as_i64()) {
                        tagged_cut = Some(seq);
                    }
                }
            }
            "assistant/message" | "assistant/attempt" | "compaction/summary" => {
                let is_attempt = kind == "assistant/attempt";
                let top = if !is_attempt {
                    data.and_then(|d| d.get("usage")).filter(|u| u.is_object())
                } else {
                    None
                };
                let usage = top.or_else(|| last_stream_usage(data.and_then(|d| d.get("stream"))));
                let Some(usage) = usage else { continue };
                let tokens = tokens_of(usage);
                if tokens.total() == 0 {
                    continue;
                }
                let time = i64_of(record.get("time")).max(0) as u64;
                if time == 0 {
                    continue;
                }
                let message = data.and_then(|d| d.get("message"));
                let source = message.and_then(|m| m.get("source"));
                let seq = record.get("seq").and_then(|s| s.as_i64());
                let message_id = str_of(message.and_then(|m| m.get("id")));
                let identity = if !message_id.is_empty() {
                    format!("msg:{message_id}")
                } else if let Some(s) = seq {
                    format!("seq:{s}")
                } else {
                    format!("sid:{header_id}")
                };
                events.push(UsageEvent {
                    record_index: index,
                    seq,
                    time,
                    provider: str_of(source.and_then(|s| s.get("provider"))),
                    model: str_of(source.and_then(|s| s.get("model"))),
                    identity,
                    tokens,
                    kind: match kind {
                        "compaction/summary" => "summary",
                        "assistant/attempt" => "attempt",
                        _ => "reply",
                    },
                });
            }
            _ => {}
        }
    }

    // v0 persisted the cut on the header as `seedLength`; current generations keep only the lineage
    // bit there and project the cut into the log as a tagged end-seed. Both name the same thing: the
    // seq of the first event the child itself owns.
    let mut inherited_cut = seed_length;
    if inherited_cut.is_none() && is_seeded {
        inherited_cut = tagged_cut;
    }
    // A seeded header with no cut anywhere in the log cannot separate the copied parent prefix from
    // the child's own work. Charging everything would bill the parent's tokens to the child, so such
    // a session is charged nothing. A log that does carry an end-seed marker is not in this position
    // — an untagged one says outright that nothing was inherited.
    let cut_unknowable = is_seeded && inherited_cut.is_none() && !seed_boundary_seen;

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut days: BTreeMap<i32, u64> = BTreeMap::new();
    for event in events {
        let cut_applies = header_index.map(|h| event.record_index > h).unwrap_or(false);
        if cut_applies && cut_unknowable {
            continue;
        }
        if cut_applies {
            if let (Some(cut), Some(seq)) = (inherited_cut, event.seq) {
                if seq < cut {
                    continue;
                }
            }
        }
        let dedup = format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}",
            event.kind,
            event.identity,
            event.time,
            event.provider,
            event.model,
            event.tokens.input,
            event.tokens.output,
            event.tokens.cache_read,
            event.tokens.cache_write
        );
        if !seen.insert(dedup) {
            continue;
        }
        *days.entry(local_day(event.time)).or_insert(0) += event.tokens.total().max(0) as u64;
    }
    let cutoff = local_day(now_ms()) - KEEP_DAYS;
    days.retain(|day, _| *day >= cutoff);
    days
}

// ---------------- the per-file cache ----------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileTotals {
    size: u64,
    mtime_ms: u64,
    /// Tokens per local day (days-from-CE), so a restart does not have to decode the whole history
    /// again.
    days: BTreeMap<i32, u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TotalsCache {
    files: HashMap<String, FileTotals>,
}

fn load_cache() -> TotalsCache {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|t| serde_json::from_str::<TotalsCache>(&t).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &TotalsCache) {
    if let Ok(t) = serde_json::to_string(cache) {
        let _ = std::fs::write(cache_path(), t);
    }
}

fn mtime_ms_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One full pass: read what changed, reuse what did not, and total every live transcript by day.
fn scan(cache: &mut TotalsCache) -> (BTreeMap<i32, u64>, usize) {
    let Some(root) = sessions_root() else { return (BTreeMap::new(), 0) };
    let files = preferred_transcripts(&root);
    let mut live: HashMap<String, FileTotals> = HashMap::new();
    let mut total: BTreeMap<i32, u64> = BTreeMap::new();

    for path in &files {
        let Ok(meta) = std::fs::metadata(path) else { continue };
        let size = meta.len();
        let mtime_ms = mtime_ms_of(&meta);
        let key = path.to_string_lossy().to_string();
        let cached = cache
            .files
            .get(&key)
            .filter(|entry| entry.size == size && entry.mtime_ms == mtime_ms)
            .cloned();
        let entry = match cached {
            Some(entry) => entry,
            None => {
                let days = read_transcript(path).map(|text| fold_transcript(&text)).unwrap_or_default();
                FileTotals { size, mtime_ms, days }
            }
        };
        for (day, tokens) in &entry.days {
            *total.entry(*day).or_insert(0) += *tokens;
        }
        live.insert(key, entry);
    }
    cache.files = live;
    (total, files.len())
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

fn read_once(prev: &UsageSnapshot, cache: &mut TotalsCache) -> UsageSnapshot {
    let mut snap = prev.clone();
    if !present() {
        return UsageSnapshot { status: "absent".into(), ..Default::default() };
    }
    let (days, file_count) = scan(cache);
    save_cache(cache);

    if file_count == 0 {
        snap.status = "none".into();
        snap.windows.clear();
        snap.note = "No DeepSeek Harness session has run yet".into();
        return snap;
    }

    let today = local_day(now_ms());
    let today_tokens = total_between(&days, today, today);
    let week_tokens = total_between(&days, today - 6, today);
    let month_tokens = total_between(&days, today - 29, today);

    snap.status = "ok".into();
    snap.fetched_at = now_ms();
    snap.note.clear();
    // "Today" is always present, so the ring always means the same thing; the wider windows only
    // appear once they have something in them.
    let mut windows = vec![count_window("today", "Today", today_tokens)];
    if week_tokens > 0 {
        windows.push(count_window("week", "Last 7 days", week_tokens));
    }
    if month_tokens > 0 {
        windows.push(count_window("month", "Last 30 days", month_tokens));
    }
    snap.windows = windows;
    snap
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.dsh.lock().unwrap() = snap.clone();
    persist(&snap);
    let _ = app.emit("dsh", &snap);
}

fn sleep_interruptible(secs: u64) {
    for _ in 0..secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut cache = load_cache();
        {
            let st = app.state::<AppState>();
            let snap = st.dsh.lock().unwrap().clone();
            let _ = app.emit("dsh", &snap);
        }
        if !present() {
            broadcast(&app, UsageSnapshot { status: "absent".into(), ..Default::default() });
            loop {
                sleep_interruptible(ABSENT_POLL_SECS);
                if present() {
                    break;
                }
            }
        }
        loop {
            // Same shape as every other provider's loop: the guard drops before `st`.
            let prev = {
                let st = app.state::<AppState>();
                let held = st.dsh.lock().unwrap().clone();
                held
            };
            let snap = read_once(&prev, &mut cache);
            broadcast(&app, snap);
            sleep_interruptible(POLL_SECS);
        }
    });
}

/// For doctor: where the transcript tree is, and what today has cost.
pub fn probe() -> String {
    let Some(home) = home() else { return "DeepSeek Harness: no home directory to look under".into() };
    let Some(root) = sessions_root() else { return "DeepSeek Harness: no home directory to look under".into() };
    if !root.is_dir() {
        return format!("DeepSeek Harness: {} not found (the harness has never run here)", root.display());
    }
    let files = preferred_transcripts(&root);
    if files.is_empty() {
        return format!("DeepSeek Harness: {} holds no session transcript yet", root.display());
    }
    let mut cache = load_cache();
    let (days, _) = scan(&mut cache);
    let today = local_day(now_ms());
    format!(
        "DeepSeek Harness: {} transcript(s) under {} (DSH_HOME={}), {} tokens today",
        files.len(),
        root.display(),
        home.display(),
        total_between(&days, today, today)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_harness_own_transcript_names_are_read() {
        assert_eq!(session_log_rank("session.jsonl"), Some(0));
        assert_eq!(session_log_rank("session.jsonl.zstd"), Some(0));
        assert_eq!(session_log_rank("session.v3.jsonl"), Some(3));
        assert_eq!(session_log_rank("session.v3.jsonl.zstd"), Some(3));
        assert_eq!(session_log_rank("session.v12.jsonl.zstd"), Some(12));
        assert_eq!(session_log_rank("session.summary.jsonl"), None);
        assert_eq!(session_log_rank("session.jsonl.bak"), None);
        assert_eq!(session_log_rank("other.jsonl"), None);
        assert_eq!(session_log_rank("session.v.jsonl"), None);
    }

    fn line(value: serde_json::Value) -> String {
        value.to_string()
    }

    fn header(extra: serde_json::Value) -> String {
        let mut object = serde_json::Map::new();
        object.insert("type".into(), serde_json::json!("session"));
        object.insert("seq".into(), serde_json::json!(0));
        object.insert("id".into(), serde_json::json!("s-1"));
        if let Some(map) = extra.as_object() {
            for (k, v) in map {
                object.insert(k.clone(), v.clone());
            }
        }
        line(serde_json::Value::Object(object))
    }

    fn reply(seq: i64, time: u64, message_id: &str, input: i64, output: i64) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant/message",
            "seq": seq,
            "time": time,
            "data": {
                "usage": { "inputTokens": input, "outputTokens": output },
                "message": { "id": message_id, "source": { "provider": "deepseek", "model": "deepseek-v4" } }
            }
        })
    }

    /// One instant, frozen at first use and always inside the retention window.
    /// A hard-coded timestamp ages out of `KEEP_DAYS`, and every fold test in this
    /// module then fails for a reason that has nothing to do with what it asserts.
    fn t() -> u64 {
        static INIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        *INIT.get_or_init(now_ms)
    }

    #[test]
    fn a_reply_contributes_its_tokens_to_the_day_it_happened() {
        let text = vec![header(serde_json::json!({})), line(reply(1, t(), "m-1", 100, 40))].join("\n");
        let days = fold_transcript(&text);
        assert_eq!(days.get(&local_day(t())), Some(&140));
        assert_eq!(days.len(), 1);
    }

    /// DSH's reasoning is a subset of output; subtracting it would under-count every
    /// reasoning-heavy session by exactly its reasoning tokens.
    #[test]
    fn reasoning_is_not_subtracted_from_the_reported_output() {
        let record = serde_json::json!({
            "type": "assistant/message",
            "seq": 1,
            "time": t(),
            "data": { "usage": { "inputTokens": 10, "outputTokens": 50, "reasoningTokens": 30 } }
        });
        let text = vec![header(serde_json::json!({})), line(record)].join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&60));
    }

    #[test]
    fn cache_tokens_count_towards_the_total() {
        let record = serde_json::json!({
            "type": "assistant/message",
            "seq": 1,
            "time": t(),
            "data": { "usage": {
                "inputTokens": 1, "outputTokens": 2, "cacheReadTokens": 3, "cacheWriteTokens": 4
            } }
        });
        let text = vec![header(serde_json::json!({})), line(record)].join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&10));
    }

    /// The writer can re-append a line it already flushed; that is not a second charge.
    #[test]
    fn a_replayed_record_is_counted_once() {
        let duplicate = reply(1, t(), "m-1", 100, 40);
        let text = vec![
            header(serde_json::json!({})),
            line(duplicate.clone()),
            line(duplicate),
        ]
        .join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&140));
    }

    /// A forked session's log opens with a copy of its parent's events; those are the parent's. The
    /// legacy header states the cut as a seq, so `seedLength: 3` drops the two parent replies and
    /// leaves the child's own.
    #[test]
    fn a_legacy_seed_cut_hides_the_inherited_prefix() {
        let text = vec![
            header(serde_json::json!({ "seedLength": 3 })),
            line(reply(1, t(), "parent-1", 100, 40)),
            line(reply(2, t(), "parent-2", 100, 40)),
            line(reply(3, t(), "child-1", 7, 3)),
        ]
        .join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&10));
    }

    /// The cut is exclusive — the event whose seq equals it is the child's own and is charged, and
    /// only the events below it are dropped. Read as an event count instead, `seedLength: 2` would
    /// swallow the child's first reply and report 10 rather than 150.
    #[test]
    fn the_seed_cut_leaves_the_event_on_it_charged() {
        let text = vec![
            header(serde_json::json!({ "seedLength": 2 })),
            line(reply(1, t(), "parent-1", 100, 40)),
            line(reply(2, t(), "parent-2", 100, 40)),
            line(reply(3, t(), "child-1", 7, 3)),
        ]
        .join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&150));
    }

    #[test]
    fn a_tagged_end_seed_hides_the_inherited_prefix() {
        let text = vec![
            header(serde_json::json!({ "isSeeded": true })),
            line(reply(1, t(), "parent-1", 100, 40)),
            line(serde_json::json!({
                "type": "session/end-seed", "seq": 2, "time": t(), "data": { "inherited": true }
            })),
            line(reply(3, t(), "child-1", 7, 3)),
        ]
        .join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&10));
    }

    /// An untagged end-seed is this log stating that nothing was inherited. It is not a cut, so the
    /// history above it is real and belongs to this session.
    #[test]
    fn an_untagged_end_seed_hides_nothing() {
        let text = vec![
            header(serde_json::json!({ "isSeeded": true })),
            line(reply(1, t(), "m-1", 100, 40)),
            line(serde_json::json!({
                "type": "session/end-seed", "seq": 2, "time": t(), "data": { "inherited": false }
            })),
        ]
        .join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&140));
    }

    /// A seeded header whose log never says where the copy ended cannot say what it owns, so it
    /// charges nothing rather than charging the copied parent prefix to the child. This is the
    /// counterpart of `an_untagged_end_seed_hides_nothing`: a log that is silent and a log that
    /// denies inheritance outright are two different answers, and only the silent one is refused.
    #[test]
    fn a_seeded_header_without_its_marker_charges_nothing() {
        let text = vec![header(serde_json::json!({ "isSeeded": true })), line(reply(1, t(), "m-1", 100, 40))]
            .join("\n");
        assert!(fold_transcript(&text).is_empty());
    }

    /// A call that never produced a surface message keeps its usage in the embedded stream.
    #[test]
    fn an_attempt_reads_its_usage_out_of_the_stream() {
        let record = serde_json::json!({
            "type": "assistant/attempt",
            "seq": 1,
            "time": t(),
            "data": { "stream": [
                { "chunk": { "type": "text", "usage": null } },
                { "chunk": { "type": "usage", "usage": { "inputTokens": 5, "outputTokens": 6 } } }
            ] }
        });
        let text = vec![header(serde_json::json!({})), line(record)].join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&11));
    }

    #[test]
    fn an_event_without_a_usable_time_is_left_out() {
        let mut record = reply(1, 0, "m-1", 100, 40);
        record["time"] = serde_json::json!(0);
        let text = vec![header(serde_json::json!({})), line(record)].join("\n");
        assert!(fold_transcript(&text).is_empty());
    }

    #[test]
    fn unrelated_events_and_junk_lines_are_ignored() {
        let text = [
            header(serde_json::json!({})).as_str(),
            "not json at all",
            "",
            &line(serde_json::json!({ "type": "user/message", "seq": 1, "time": t(), "data": {} })),
            &line(reply(2, t(), "m-1", 1, 1)),
        ]
        .join("\n");
        assert_eq!(fold_transcript(&text).get(&local_day(t())), Some(&2));
    }

    #[test]
    fn a_zero_token_event_is_not_a_reading() {
        let text = vec![header(serde_json::json!({})), line(reply(1, t(), "m-1", 0, 0))].join("\n");
        assert!(fold_transcript(&text).is_empty());
    }

    #[test]
    fn a_folded_transcript_drops_days_past_the_keep_window() {
        let old = now_ms() - (KEEP_DAYS as u64 + 5) * 24 * 60 * 60 * 1000;
        let text = vec![header(serde_json::json!({})), line(reply(1, old, "m-old", 10, 10))].join("\n");
        assert!(fold_transcript(&text).is_empty());
    }

    #[test]
    fn a_day_range_is_inclusive_at_both_ends() {
        let mut days: BTreeMap<i32, u64> = BTreeMap::new();
        days.insert(100, 1);
        days.insert(101, 2);
        days.insert(102, 4);
        assert_eq!(total_between(&days, 101, 102), 6);
        assert_eq!(total_between(&days, 100, 100), 1);
        assert_eq!(total_between(&days, 103, 104), 0);
    }

    /// A frame boundary scan must not be fooled into reading a torn tail as a frame.
    #[test]
    fn a_truncated_frame_is_not_reported_as_one() {
        let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x00];
        assert!(scan_zstd_frames(&frame).is_empty());
        let not_a_magic = [0x00, 0x00, 0x00, 0x00];
        assert!(scan_zstd_frames(&not_a_magic).is_empty());
    }
}
