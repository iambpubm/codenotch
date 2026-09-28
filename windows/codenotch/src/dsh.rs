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

use crate::usage::{CostEstimate, CostPart, LimitWindow, UsageSnapshot};
use crate::AppState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
/// The four bytes every zstd frame opens with — `28 B5 2F FD` — read as the little-endian word the
/// scan compares against. Written as one hex run rather than as reordered bytes because that is the
/// form the format is specified in, and a transposed nibble here is not a compile error, not a
/// visible fault, and not a failed test: it makes the scan find zero frames, so a provider with
/// real transcripts on disk reports nothing at all while looking perfectly healthy.
const ZSTD_MAGIC: u32 = 0xFD2F_B528;
/// Days kept in the per-file cache: past this nothing is reported, so nothing needs keeping.
const KEEP_DAYS: i32 = 31;
/// Bumped whenever the fold changes what a transcript is worth. A cache an older reader wrote is
/// not merely out of date — it is wrong in a way that nothing downstream can detect, because the
/// whole point of the cache is to skip the read that would reveal it. Discarding a mismatch costs
/// one re-read, and is the only safe answer.
///
/// Version 2: the reader that wrote version 1 could not see past a transcript's first frame, so
/// every entry in such a cache totals a whole session as nothing.
///
/// Version 3: a file's totals became a day → model → tokens map so spend can be priced per model and
/// per time-of-day band. A version 2 entry holds bare per-day token counts, which carry neither, so
/// it cannot be reinterpreted — the type changed, and the old shape must not survive as a plausible
/// reading with the money missing.
const CACHE_SCHEMA: u32 = 3;

/// China Standard Time has no daylight saving: a fixed UTC+8, all year.
const BEIJING_OFFSET_SECS: i64 = 8 * 3600;

/// A model's price, in the vendor's currency per million tokens.
///
/// Kept as three separate rates because the vendor's own spread is enormous: DeepSeek charges fifty
/// times more for a cache-miss input token than a cache-hit one, and four times more again for an
/// output token. A single blended rate would be wrong for every session, in both directions.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RateCard {
    hit: f64,
    miss: f64,
    output: f64,
}

/// DeepSeek's published rate card, in CNY per million tokens, read from its "Models & Pricing" page
/// on 2026-09-28. Off-peak and peak are both written out rather than one derived from the other:
/// the doubling is the vendor's current rule and not a law of nature, and a future card that breaks
/// it should not need this file rewritten to be correct.
const FLASH_OFF: RateCard = RateCard { hit: 0.02, miss: 1.0, output: 4.0 };
const FLASH_PEAK: RateCard = RateCard { hit: 0.04, miss: 2.0, output: 8.0 };
const PRO_OFF: RateCard = RateCard { hit: 0.15, miss: 4.5, output: 13.5 };
const PRO_PEAK: RateCard = RateCard { hit: 0.30, miss: 9.0, output: 27.0 };

/// The rate card for a model name, as (off-peak, peak).
///
/// `None` for a model this build does not know, and that is deliberate: the caller drops such usage
/// from every figure and names it instead. Pricing an unknown model off a sibling's card would
/// produce a number that looks authoritative and is simply not the one the vendor would bill.
///
/// `deepseek-v4-flash` and `deepseek-v4-flash-vision-exp` are the vendor's own retired aliases —
/// requests to them are served by the Flash model and billed at Flash rates — so they share its card.
/// The `-pro` prefix is checked first because it is the longer, more specific name.
fn rate_card(model: &str) -> Option<(RateCard, RateCard)> {
    let m = model.trim().to_ascii_lowercase();
    if m.starts_with("deepseek-v4-pro") || m.starts_with("deepseek-pro") {
        return Some((PRO_OFF, PRO_PEAK));
    }
    if m.starts_with("deepseek-flash")
        || m.starts_with("deepseek-v4-flash")
        || m.starts_with("deepseek-v4.1-flash")
    {
        return Some((FLASH_OFF, FLASH_PEAK));
    }
    None
}

/// Whether a transcript timestamp falls in a peak-priced hour.
///
/// The vendor's rule: Beijing time, Monday to Friday, 09:00–12:00 and 14:00–18:00 are peak; every
/// other hour — including weekends and public holidays — is off-peak, at half the price.
///
/// Public holidays are treated as ordinary weekdays here. Which days are holidays is published
/// afresh every year, so a table baked into this build is guaranteed to expire, and the mistake is
/// one-directional: reading a holiday as a peak weekday overstates those days' cost, while the
/// reverse understates it. Overstating is the safe direction for an estimate.
fn is_peak(ms: u64) -> bool {
    let secs = (ms / 1000) as i64 + BEIJING_OFFSET_SECS;
    let days = secs.div_euclid(86_400);
    let hour = secs.rem_euclid(86_400) / 3600;
    // 1970-01-01 was a Thursday, so shifting by 3 makes 0 mean Monday.
    let weekday = (days + 3).rem_euclid(7);
    if weekday >= 5 {
        return false;
    }
    (9..12).contains(&hour) || (14..18).contains(&hour)
}

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
    // A key is enough on its own. The balance comes from DeepSeek's own endpoint and needs no
    // transcript, so a machine that has DeepSeek set up but has not run the harness yet still has
    // something true to show — and that is the case where the balance is the *only* thing there is.
    // Without a key the harness's session tree is the whole of the reading.
    sessions_root().map(|p| p.is_dir()).unwrap_or(false) || crate::deepseek::key_available()
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

// ---------------- whether a turn is still open ----------------

/// The four log entries that decide whether a session is still working.
///
/// Everything else the harness writes is either inside a turn (`step/start`, `tool/call`,
/// `assistant/message`, `tool/result`, …) or bookkeeping (`session/title`, `request/header`,
/// `permission/preset`, …), so the *last* of these four is the whole answer: whichever one comes
/// last is the state. There is no voting and no window.
///
/// `turn/end` is why this is state rather than a guess. The harness writes it in a `finally` in the
/// turn loop of `dsh-agent-loop`, so it lands whatever ended the turn — a conclusion (`completed`),
/// a rejected pre-step (`blocked`), a token ceiling, an abort (`aborted`), an error (`error`). A
/// turn cannot be over without the log saying so, which is what lets this reader do without the
/// silence thresholds the Codex one needs. The only way to get a stale `turn/start` is for the
/// process to die before its `finally` runs, and `activity.rs` covers that with an age check.
///
/// The `reason.kind` that comes with that `turn/end` is read as well, because the four edges say
/// only *that* a turn ended while the notch has a fifth thing to draw: a turn that just finished.
/// This is the only provider on the machine that states that outright instead of leaving it to be
/// inferred from silence — see `State::completed`.
///
/// `approval/asked` / `approval/decided` are written as a pair, one `asked` and the `decided` that
/// always follows it, so an `asked` with no `decided` after it is a question still on screen. That
/// is the same fact the notch draws in amber.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// `turn/start` — a turn began and has not ended.
    TurnOpen,
    /// `turn/end` — the last turn is over.
    TurnClosed,
    /// `approval/asked` — a tool is waiting for your permission.
    ApprovalAsked,
    /// `approval/decided` — you answered, and the turn carries on.
    ApprovalSettled,
}

fn edge_of(kind: &str) -> Option<Edge> {
    match kind {
        "turn/start" => Some(Edge::TurnOpen),
        "turn/end" => Some(Edge::TurnClosed),
        "approval/asked" => Some(Edge::ApprovalAsked),
        "approval/decided" => Some(Edge::ApprovalSettled),
        _ => None,
    }
}

/// One edge as the log wrote it: which one, when, and — for a `turn/end` — why the turn ended.
struct EdgeHit {
    edge: Edge,
    at: u64,
    /// The `turn/end`'s `reason.kind`. `None` for the other three edges, which carry no reason.
    reason: Option<String>,
}

/// The last edge written in one block of decoded lines, with its time. The walk is backwards
/// because only the last one matters, and stopping at the first hit is what keeps it cheap.
fn edge_in(text: &str) -> Option<EdgeHit> {
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
        let Some(edge) = record.get("type").and_then(|t| t.as_str()).and_then(edge_of) else { continue };
        let at = record.get("time").and_then(|t| t.as_i64()).unwrap_or(0).max(0) as u64;
        // `turn/end` is the one edge that explains itself. `TurnEndReason` in the harness's own type
        // declarations is the domain: `completed` is a turn that finished its work, and `aborted`,
        // `blocked`, `error`, `max-tokens` and the crash repair the loop writes for a log whose last
        // turn never closed are all endings of a different kind. Reading it is what lets the notch
        // draw a green ring for the first and nothing at all for the rest.
        let reason = (edge == Edge::TurnClosed)
            .then(|| str_of(record.pointer("/data/reason/kind")))
            .filter(|kind| !kind.is_empty());
        return Some(EdgeHit { edge, at, reason });
    }
    None
}

/// What a session log says about itself: whether it is mid-turn, and what to call it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// The last edge the log wrote. `None` for a log that has not started a turn yet.
    pub edge: Option<Edge>,
    /// When that edge was written, in ms since the epoch. Zero when there is none.
    pub at: u64,
    /// Why the last turn ended, when the last edge was `turn/end`. `None` when no turn has ended,
    /// and also for the three edges that have nothing to explain.
    pub outcome: Option<String>,
    /// The title the session gave itself, empty when it never wrote one.
    pub title: String,
    /// The directory the session runs in, from the log's own header.
    pub cwd: String,
}

impl State {
    /// A turn is open — the log's last word on the subject was `turn/start`, or an approval that was
    /// answered and left the turn running.
    pub fn working(&self) -> bool {
        matches!(self.edge, Some(Edge::TurnOpen) | Some(Edge::ApprovalSettled))
    }

    /// A tool asked for permission and nothing has answered it yet.
    pub fn waiting(&self) -> bool {
        matches!(self.edge, Some(Edge::ApprovalAsked))
    }

    /// The last turn ran to its end rather than being cut short.
    ///
    /// Only `completed` counts. A turn that was aborted, that failed, that hit its output ceiling,
    /// or that never closed at all and was repaired after a crash ended some other way, and none of
    /// those is a piece of work finishing — a green ring on any of them would say the opposite of
    /// what happened. The distinction is the harness's own, so it is read rather than inferred.
    pub fn completed(&self) -> bool {
        matches!(self.edge, Some(Edge::TurnClosed)) && self.outcome.as_deref() == Some("completed")
    }
}

/// How far back the walk will go before giving up. A long-running turn writes a frame per flush, so
/// hundreds of them can sit between its `turn/start` and the end of the file; the bound exists so a
/// pathological log cannot be decompressed end to end on every tick. Hitting it means "no edge
/// found", which `working()` reads as idle — the opposite of generous, and deliberately so: an
/// unreadable log is not evidence that an agent is running.
const EDGE_SCAN_FRAMES: usize = 400;

/// How far into the log the title and the working directory are looked for. The harness writes both
/// in the opening frames, well before the first turn begins.
const HEAD_SCAN_FRAMES: usize = 16;

/// Pick the header and the title out of a block of decoded lines. Later wins, so a re-stated title
/// leaves the newest one in place.
fn head_of(text: &str, state: &mut State) {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
        match record.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "session" => {
                let cwd = str_of(record.get("cwd"));
                if !cwd.is_empty() {
                    state.cwd = cwd;
                }
            }
            "session/title" => {
                let title = str_of(record.pointer("/data/title"));
                if !title.is_empty() {
                    state.title = title;
                }
            }
            _ => {}
        }
    }
}

/// Read a session log's own account of whether it is still working.
///
/// One file read serves both ends: the tail is walked backwards for the last edge, the head forwards
/// for the title and the directory. Reading the whole file is what makes the tail walk possible at
/// all — Zstandard frames can only be located by walking from a frame boundary, so there is no way
/// to seek to the end and start there.
pub fn read_state(path: &Path) -> Option<State> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_TRANSCRIPT_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mut state = State::default();
    if !path.to_string_lossy().ends_with(".jsonl.zstd") {
        let text = String::from_utf8_lossy(&bytes);
        if let Some(hit) = edge_in(&text) {
            state.edge = Some(hit.edge);
            state.at = hit.at;
            state.outcome = hit.reason;
        }
        head_of(&text, &mut state);
        return Some(state);
    }
    let frames = scan_zstd_frames(&bytes);
    for (start, end) in frames.iter().rev().take(EDGE_SCAN_FRAMES) {
        if let Some(hit) = edge_in(&decode_frame_lossy(&bytes[*start..*end])) {
            state.edge = Some(hit.edge);
            state.at = hit.at;
            state.outcome = hit.reason;
            break;
        }
    }
    for (start, end) in frames.iter().take(HEAD_SCAN_FRAMES) {
        head_of(&decode_frame_lossy(&bytes[*start..*end]), &mut state);
    }
    Some(state)
}

/// Every session's live transcript, for the activity probe. The same discovery the token walk uses,
/// exposed because "is anything running" starts from exactly the same set of files.
pub fn session_logs() -> Vec<PathBuf> {
    sessions_root().map(|root| preferred_transcripts(&root)).unwrap_or_default()
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

/// One billable event's tokens, split the way the vendor bills them.
///
/// Verified against 72 real events in this machine's transcripts: `inputTokens` and
/// `cacheReadTokens` are disjoint, and `totalTokens` is their sum plus `outputTokens` — so the
/// uncached input is `inputTokens` itself, not `inputTokens - cacheReadTokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct Charge {
    /// Input that missed the cache (`inputTokens`). The expensive half.
    miss: u64,
    /// Input served from the cache (`cacheReadTokens`). Fifty times cheaper on DeepSeek, which is why
    /// the split is kept rather than folded into `miss`.
    hit: u64,
    /// Cache writes (`cacheWriteTokens`). DeepSeek does not bill these apart — a token written to the
    /// cache was already counted as a miss — so they ride with `miss`. Always zero in the harness's
    /// current transcripts, kept so a future one that starts writing them is still priced right.
    write: u64,
    /// Output, reasoning included as a subset, exactly as `Tokens::total` counts it.
    output: u64,
}

impl Charge {
    fn of(tokens: &Tokens) -> Charge {
        Charge {
            miss: tokens.input.max(0) as u64,
            hit: tokens.cache_read.max(0) as u64,
            write: tokens.cache_write.max(0) as u64,
            output: tokens.output.max(0) as u64,
        }
    }

    fn total(&self) -> u64 {
        self.miss + self.hit + self.write + self.output
    }

    fn merge(&mut self, other: &Charge) {
        self.miss += other.miss;
        self.hit += other.hit;
        self.write += other.write;
        self.output += other.output;
    }
}

/// One model's day of usage, with the peak band kept apart from the off-peak one.
///
/// The split has to happen on the event, never on the day: peak hours cover only part of a weekday,
/// so the same day's tokens price differently depending on when inside it they were spent, and once
/// they are summed into one figure that information is gone for good.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct DayCharge {
    peak: Charge,
    off: Charge,
}

impl DayCharge {
    fn total(&self) -> u64 {
        self.peak.total() + self.off.total()
    }

    fn merge(&mut self, other: &DayCharge) {
        self.peak.merge(&other.peak);
        self.off.merge(&other.off);
    }
}

/// Everything the machine's transcripts add up to: local day → model → that model's day.
///
/// The model is part of the key because the rate card is per model. A day on its own can be counted
/// but not priced, and the previous shape — a bare day → token count — is exactly why the cache
/// schema had to be bumped rather than migrated.
type DayIndex = BTreeMap<i32, BTreeMap<String, DayCharge>>;

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

/// Totals per local day for one transcript, split by model and by pricing band. Only complete,
/// non-inherited, de-duplicated charges are in here; everything the harness itself would not count
/// is filtered out before it lands.
fn fold_transcript(text: &str) -> DayIndex {
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
    let mut days: DayIndex = BTreeMap::new();
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
        let charge = Charge::of(&event.tokens);
        let slot = days
            .entry(local_day(event.time))
            .or_default()
            .entry(event.model.clone())
            .or_default();
        if is_peak(event.time) {
            slot.peak.merge(&charge);
        } else {
            slot.off.merge(&charge);
        }
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
    /// Tokens per local day (days-from-CE), per model and pricing band, so a restart does not have to
    /// decode the whole history again.
    days: DayIndex,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TotalsCache {
    /// Absent from a cache written before this field existed, which is precisely the case that has
    /// to be rejected: those entries were produced by the reader that could not see past a frame
    /// header, and each one states that a transcript holding real history is worth nothing.
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    files: HashMap<String, FileTotals>,
}

/// Hold a cache to the schema this reader writes. Split out from the file read so the rule can be
/// exercised without a disk.
fn usable(cache: TotalsCache) -> TotalsCache {
    if cache.schema == CACHE_SCHEMA {
        cache
    } else {
        TotalsCache { schema: CACHE_SCHEMA, ..Default::default() }
    }
}

fn load_cache() -> TotalsCache {
    usable(
        std::fs::read_to_string(cache_path())
            .ok()
            .and_then(|t| serde_json::from_str::<TotalsCache>(&t).ok())
            .unwrap_or_default(),
    )
}

fn save_cache(cache: &mut TotalsCache) {
    cache.schema = CACHE_SCHEMA;
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
///
/// One session can be spread over several transcripts, so the per-file days are merged — per day and
/// per model, with the pricing bands added together component by component.
fn scan(cache: &mut TotalsCache) -> (DayIndex, usize) {
    let Some(root) = sessions_root() else { return (BTreeMap::new(), 0) };
    let files = preferred_transcripts(&root);
    let mut live: HashMap<String, FileTotals> = HashMap::new();
    let mut total: DayIndex = BTreeMap::new();

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
        for (day, models) in &entry.days {
            let slot = total.entry(*day).or_default();
            for (model, charge) in models {
                slot.entry(model.clone()).or_default().merge(charge);
            }
        }
        live.insert(key, entry);
    }
    cache.files = live;
    (total, files.len())
}

fn total_between(days: &DayIndex, from_day: i32, to_day: i32) -> u64 {
    days.range(from_day..=to_day)
        .map(|(_, models)| models.values().map(DayCharge::total).sum::<u64>())
        .sum()
}

/// What a transcript that names no model is called when it has to be shown to someone.
const UNLABELLED_MODEL: &str = "(unlabelled)";

/// What a span of history comes to in money, and how it breaks down.
#[derive(Debug, Clone, Default, PartialEq)]
struct Quote {
    total: f64,
    hit_tokens: u64,
    hit_cost: f64,
    miss_tokens: u64,
    miss_cost: f64,
    output_tokens: u64,
    output_cost: f64,
    /// Models that carried tokens but have no rate card here. Their usage is left out of every figure
    /// rather than priced by guesswork, and naming them is how the card says why a total may read
    /// lower than the token count above it suggests.
    unpriced: BTreeSet<String>,
}

impl Quote {
    fn absorb(&mut self, charge: &Charge, rates: &RateCard) {
        let per_million = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
        self.hit_tokens += charge.hit;
        self.hit_cost += per_million(charge.hit, rates.hit);
        // A cache write rides with the misses: the vendor bills that token once, at the miss rate.
        let miss = charge.miss + charge.write;
        self.miss_tokens += miss;
        self.miss_cost += per_million(miss, rates.miss);
        self.output_tokens += charge.output;
        self.output_cost += per_million(charge.output, rates.output);
    }

    /// The total is only ever the sum of the parts, computed in one place so a caller cannot report a
    /// total that disagrees with the rows above it.
    fn settle(&mut self) {
        self.total = self.hit_cost + self.miss_cost + self.output_cost;
    }
}

/// Price every model's usage between two local days, inclusive. Each band is priced at its own rate
/// card, and an unknown model contributes nothing but its name.
fn quote(days: &DayIndex, from_day: i32, to_day: i32) -> Quote {
    let mut q = Quote::default();
    for (_, models) in days.range(from_day..=to_day) {
        for (model, charge) in models {
            let Some((off, peak)) = rate_card(model) else {
                q.unpriced.insert(if model.trim().is_empty() {
                    UNLABELLED_MODEL.to_string()
                } else {
                    model.clone()
                });
                continue;
            };
            q.absorb(&charge.off, &off);
            q.absorb(&charge.peak, &peak);
        }
    }
    q.settle();
    q
}

fn count_window(id: &str, label: &str, tokens: u64, cost: f64) -> LimitWindow {
    LimitWindow {
        id: id.into(),
        label: label.into(),
        used: 0.0,
        resets_at: None,
        count: Some(tokens as i64),
        unit: Some("tokens".into()),
        cost: Some(cost),
        ..Default::default()
    }
}

/// The money side of a reading: the widest span's breakdown, the rate card's terms, and whichever
/// model was left out. Built from the same `Quote` the windows were, so the rows on the card and the
/// figures beside them cannot drift apart.
fn cost_estimate(q: &Quote) -> CostEstimate {
    let mut unpriced: Vec<String> = q.unpriced.iter().cloned().collect();
    unpriced.sort();
    CostEstimate {
        currency: "CNY".into(),
        parts: vec![
            CostPart { label: "Cache-hit input".into(), tokens: q.hit_tokens as i64, cost: q.hit_cost },
            CostPart { label: "Cache-miss input".into(), tokens: q.miss_tokens as i64, cost: q.miss_cost },
            CostPart { label: "Output".into(), tokens: q.output_tokens as i64, cost: q.output_cost },
        ],
        rate_note: "Estimated from this machine's transcripts and DeepSeek's published rates. \
                    Weekday peak hours cost double."
            .into(),
        estimated: true,
        window: "Last 30 days".into(),
        unpriced,
    }
}

/// The account balance, read from the vendor, or the last one that could be read.
///
/// A request that fails must not empty a figure that was correct a minute ago: the token totals
/// beside it are local and always answer, so a balance blinking out would read as the account having
/// been drained rather than as one call having failed. The previous window is therefore the fallback,
/// and only a machine that has never read a balance shows none.
fn balance_window(prev: &UsageSnapshot) -> Option<LimitWindow> {
    let (key, _) = crate::deepseek::resolved_key()?;
    match crate::deepseek::fetch(&key) {
        Ok(balance) => Some(crate::deepseek::window(&balance, crate::deepseek::topup_total())),
        Err(_) => prev.windows.iter().find(|w| w.id == "balance").cloned(),
    }
}

fn read_once(prev: &UsageSnapshot, cache: &mut TotalsCache) -> UsageSnapshot {
    let mut snap = prev.clone();
    if !present() {
        return UsageSnapshot { status: "absent".into(), ..Default::default() };
    }
    let (days, file_count) = scan(cache);
    save_cache(cache);

    let balance = balance_window(prev);

    if file_count == 0 {
        // No transcript here. The balance alone is still a reading — money is money whether or not
        // the harness has been run — but with nothing to price there is no money block to show.
        let alone = balance.is_some();
        snap.windows = balance.into_iter().collect();
        snap.cost = None;
        snap.fetched_at = now_ms();
        snap.status = if alone { "ok".into() } else { "none".into() };
        snap.note = "No DeepSeek Harness session has run yet".into();
        return snap;
    }

    let today = local_day(now_ms());
    let today_tokens = total_between(&days, today, today);
    let week_tokens = total_between(&days, today - 6, today);
    let month_tokens = total_between(&days, today - 29, today);
    let today_quote = quote(&days, today, today);
    let week_quote = quote(&days, today - 6, today);
    let month_quote = quote(&days, today - 29, today);

    snap.status = "ok".into();
    snap.fetched_at = now_ms();
    snap.note.clear();
    // The balance leads: it is money, and it is the only figure here that came from the vendor
    // rather than from this machine's own arithmetic. "Today" is always present, so the ring always
    // means the same thing; the wider windows only appear once they have something in them.
    let mut windows: Vec<LimitWindow> = balance.into_iter().collect();
    windows.push(count_window("today", "Today", today_tokens, today_quote.total));
    if week_tokens > 0 {
        windows.push(count_window("week", "Last 7 days", week_tokens, week_quote.total));
    }
    if month_tokens > 0 {
        windows.push(count_window("month", "Last 30 days", month_tokens, month_quote.total));
    }
    snap.windows = windows;
    // The breakdown is the widest span's, which is the one the card shows last and the only one whose
    // rows still add up to something a person would want to read in full.
    snap.cost = Some(cost_estimate(&month_quote));
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

/// For doctor: where the transcript tree is, what today has cost, and what the account holds.
pub fn probe() -> String {
    let transcripts = transcript_probe();
    format!("{}\n{}", transcripts, crate::deepseek::probe())
}

fn transcript_probe() -> String {
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
                "message": { "id": message_id, "source": { "provider": "deepseek-official", "model": "deepseek-flash" } }
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

    /// The tokens a day holds, across every model and both pricing bands. A fold test asking "how
    /// much did that day come to" is asking for exactly this sum.
    fn day_total(days: &DayIndex, day: i32) -> u64 {
        days.get(&day).map(|models| models.values().map(DayCharge::total).sum()).unwrap_or(0)
    }

    /// One model's tokens across the whole index, both bands. A fold test asserting about the *bands*
    /// rather than about a day sums this way, which keeps it clear of the question of which local day
    /// an instant lands on — that depends on the machine's timezone, and the band does not.
    fn model_charge(days: &DayIndex, model: &str) -> DayCharge {
        let mut total = DayCharge::default();
        for models in days.values() {
            if let Some(charge) = models.get(model) {
                total.merge(charge);
            }
        }
        total
    }

    /// A day index carrying one model's miss tokens, for the tests that only care which days a span
    /// covers. Priced under `deepseek-flash`, the model this build's transcripts actually name.
    fn days_of(entries: &[(i32, u64)]) -> DayIndex {
        entries
            .iter()
            .map(|(day, tokens)| {
                let charge =
                    DayCharge { off: Charge { miss: *tokens, ..Default::default() }, ..Default::default() };
                (*day, BTreeMap::from([("deepseek-flash".to_string(), charge)]))
            })
            .collect()
    }

    /// A one-day index over the given models, so a pricing test can name its models inline.
    fn quote_of(models: &[(&str, DayCharge)]) -> Quote {
        let day: BTreeMap<String, DayCharge> =
            models.iter().map(|(m, c)| ((*m).to_string(), *c)).collect();
        let index = BTreeMap::from([(local_day(t()), day)]);
        quote(&index, local_day(t()), local_day(t()))
    }

    /// Epoch ms for a Beijing wall-clock instant, so the peak-hour tests read as the vendor's own
    /// words instead of as arithmetic on a UTC offset.
    fn beijing_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
        use chrono::{NaiveDate, TimeZone, Utc};
        let naive = NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_opt(hour, minute, 0))
            .expect("a real calendar instant");
        (Utc.from_utc_datetime(&naive).timestamp_millis() - BEIJING_OFFSET_SECS * 1000) as u64
    }

    /// A Beijing wall-clock instant on the most recent weekday at or before now.
    ///
    /// A test that needs its records to *survive the fold* cannot name a calendar date: it would read
    /// correctly the day it was written and, once that day fell outside the keep window, be asserting
    /// about a day the reader was right to have discarded. Anchoring to the same frozen "now" the fold
    /// measures against keeps the two in step for as long as the suite lives.
    fn recent_weekday_ms(hour: u32, minute: u32) -> u64 {
        let now_secs = (t() / 1000) as i64 + BEIJING_OFFSET_SECS;
        let mut day = now_secs.div_euclid(86_400);
        // 1970-01-01 was a Thursday, so shifting by 3 makes 0 mean Monday.
        while (day + 3).rem_euclid(7) >= 5 {
            day -= 1;
        }
        let secs = day * 86_400 + hour as i64 * 3600 + minute as i64 * 60;
        ((secs - BEIJING_OFFSET_SECS) * 1000) as u64
    }

    #[test]
    fn a_reply_contributes_its_tokens_to_the_day_it_happened() {
        let text = vec![header(serde_json::json!({})), line(reply(1, t(), "m-1", 100, 40))].join("\n");
        let days = fold_transcript(&text);
        assert_eq!(day_total(&days, local_day(t())), 140);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 60);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 10);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 140);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 10);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 150);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 10);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 140);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 11);
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
        assert_eq!(day_total(&fold_transcript(&text), local_day(t())), 2);
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
        let days = days_of(&[(100, 1), (101, 2), (102, 4)]);
        assert_eq!(total_between(&days, 101, 102), 6);
        assert_eq!(total_between(&days, 100, 100), 1);
        assert_eq!(total_between(&days, 103, 104), 0);
    }

    /// The opening bytes of a real transcript, taken from
    /// `~/.dsh/sessions/<project>/<session>/session.v3.jsonl.zstd` — **one frame per line**, in the
    /// order the harness appended them. Two frames rather than one because the count is the whole
    /// point: a transcript is a concatenation of frames, and a reader that stops at the first one
    /// comes back with the 191-byte header of a 94 KB file.
    const REAL_TRANSCRIPT_PREFIX: &str = "\
28b52ffd0458c50400028b23205069ab0343d197ffe1939fa38decaf632c9635852259228366e7e59399da841e86056bf394791b9ddec73703cb5cf17c52da601b4718c6b24ff1c5d5e771f194b45e7f82b8f8b82081381720e05c224e2b02f4610d711c892d95da3aa437a1dff706faf84d4128a529d520436d3d418b4185d2520c31754894e47848171f806c7ee06386b1c4f8e8f41b096b8502003eb7685b9a0b82e407\
28b52ffd0458550400f2c71a1c60a9da806e0c223492cd3acc7e2a08e2950ce3026b3b858b204bc003c57b6f3a8f2b202dcc59369c446c63ce01a2cc3e46620d0c945dbc7775ac45005d92bb691b1d3fe1d3257395246bad83a0088e8282e0f561eeeac01fb4afeeb4b5b1012deb664f533e5e59729f020b00516500db0165bb784b0b629f120636065470f1d230681662a6cc31c21835";

    /// Read the fixture above. Kept as text rather than a `&[u8]` literal so the bytes stay in the
    /// order they appear on disk, which is what makes them checkable against a hex dump.
    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// The magic number is the one thing that has to be right for a transcript to be read at all.
    /// Tying it to the literal the format defines is the assertion that catches a transcription
    /// error in it, and no behavioural test can: a wrong constant makes every scan come back empty,
    /// which is exactly what a machine with no sessions looks like too.
    #[test]
    fn the_magic_is_the_word_zstd_defines() {
        assert_eq!(ZSTD_MAGIC, u32::from_le_bytes([0x28, 0xB5, 0x2F, 0xFD]));
    }

    /// Every appended frame must be located, and the records inside both must survive decoding.
    #[test]
    fn a_real_transcripts_frames_are_all_found_and_decoded() {
        let bytes = hex(REAL_TRANSCRIPT_PREFIX);
        assert_eq!(bytes.len(), 316, "the fixture is two frames");

        let frames = scan_zstd_frames(&bytes);
        assert_eq!(frames.len(), 2, "a scan must find every frame, not only the first");
        assert_eq!(frames[0].0, 0);
        assert_eq!(frames[1].1, bytes.len(), "the scan must run to the end of the last frame");

        let text = decode_transcript(&bytes, true);
        assert!(text.contains(r#""type":"session""#), "the header record is missing");
        assert!(
            text.contains(r#""type":"permission/preset""#),
            "the record carried by the second frame is missing"
        );
    }

    /// A cache that a reader unable to see past a frame header wrote states that every transcript is
    /// worth nothing — and reuse would keep it stating that, because the files never change again
    /// and the size and mtime keys still match. Discarding on a schema miss is what lets a corrected
    /// reader get back to history it has already walked once.
    #[test]
    fn a_cache_written_by_an_older_reader_is_discarded() {
        let stale: TotalsCache = serde_json::from_str(
            r#"{"files":{"C:\\x\\session.v3.jsonl.zstd":{"size":94575,"mtime_ms":1,"days":{}}}}"#,
        )
        .expect("a cache carrying no schema must still parse, or the discard never gets to run");
        assert_eq!(stale.schema, 0, "a missing schema reads as zero");
        assert!(!stale.files.is_empty(), "the fixture is pointless without an entry to drop");

        let kept = usable(stale);
        assert!(kept.files.is_empty(), "an entry from an older schema must not survive the load");
        assert_eq!(kept.schema, CACHE_SCHEMA, "what is accepted carries the current schema");
    }

    /// The other half of the rule: what this reader wrote is reused as it stands.
    #[test]
    fn a_cache_written_by_this_reader_is_kept() {
        let entry = FileTotals { size: 9, mtime_ms: 8, days: days_of(&[(7, 6)]) };
        let current =
            TotalsCache { schema: CACHE_SCHEMA, files: HashMap::from([("k".to_string(), entry)]) };
        assert_eq!(usable(current).files.len(), 1);
    }

    /// A frame boundary scan must not be fooled into reading a torn tail as a frame.
    #[test]
    fn a_truncated_frame_is_not_reported_as_one() {
        let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x00];
        assert!(scan_zstd_frames(&frame).is_empty());
        let not_a_magic = [0x00, 0x00, 0x00, 0x00];
        assert!(scan_zstd_frames(&not_a_magic).is_empty());
    }

    // ---------------- what the tokens cost ----------------

    /// The vendor's peak windows, in Beijing time. 2026-09-28 is a Monday.
    #[test]
    fn peak_hours_are_the_vendors_own_weekday_windows() {
        let monday = |h, m| beijing_ms(2026, 9, 28, h, m);
        assert!(is_peak(monday(10, 30)), "10:30 on a Monday is inside the morning window");
        assert!(is_peak(monday(14, 0)), "14:00 opens the afternoon window");
        assert!(!is_peak(monday(8, 59)), "08:59 is before the morning window");
        assert!(!is_peak(monday(12, 0)), "12:00 ends the morning window");
        assert!(!is_peak(monday(13, 0)), "13:00 is the lunch gap");
        assert!(!is_peak(monday(18, 0)), "18:00 closes the afternoon window");
        // The weekend half of the rule, which a weekday-only implementation gets wrong.
        assert!(!is_peak(beijing_ms(2026, 9, 27, 10, 0)), "Sunday 10:00");
        assert!(!is_peak(beijing_ms(2026, 10, 3, 15, 0)), "Saturday 15:00");
        // And a weekday outside the windows is not peak either.
        assert!(!is_peak(beijing_ms(2026, 9, 25, 22, 0)), "Friday 22:00");
    }

    /// The arithmetic the card shows, checked against figures written out longhand rather than
    /// against the code's own helpers: 1M of each kind, off-peak Flash = 0.02 + 1.00 + 4.00.
    #[test]
    fn spend_is_the_rate_card_applied_to_each_kind_of_token() {
        let charge = Charge { hit: 1_000_000, miss: 1_000_000, write: 0, output: 1_000_000 };
        let q = quote_of(&[("deepseek-flash", DayCharge { off: charge, ..Default::default() })]);
        assert!((q.total - 5.02).abs() < 1e-9, "total was {}", q.total);
        assert_eq!(q.hit_tokens, 1_000_000);
        assert!((q.hit_cost - 0.02).abs() < 1e-9, "hit was {}", q.hit_cost);
        assert!((q.miss_cost - 1.0).abs() < 1e-9, "miss was {}", q.miss_cost);
        assert!((q.output_cost - 4.0).abs() < 1e-9, "output was {}", q.output_cost);
    }

    /// Peak hours cost double, and the band a charge belongs to has to survive the fold — otherwise
    /// the doubling could never be applied to anything.
    #[test]
    fn the_peak_band_costs_double_and_survives_the_fold() {
        let charge = Charge { hit: 0, miss: 1_000_000, write: 0, output: 0 };
        let off = quote_of(&[("deepseek-flash", DayCharge { off: charge, ..Default::default() })]);
        let peak = quote_of(&[("deepseek-flash", DayCharge { peak: charge, ..Default::default() })]);
        assert!((off.total - 1.0).abs() < 1e-9, "off-peak was {}", off.total);
        assert!((peak.total - 2.0).abs() < 1e-9, "peak was {}", peak.total);

        // The same two instants through the real fold: 10:00 is inside the morning window, 20:00 the
        // same weekday is outside both. Anchored to now rather than to a calendar date, because the
        // fold discards anything past the keep window and a named date would quietly stop being read.
        let morning = recent_weekday_ms(10, 0);
        let evening = recent_weekday_ms(20, 0);
        let text = vec![
            header(serde_json::json!({})),
            line(reply(1, morning, "m-1", 100, 0)),
            line(reply(2, evening, "m-2", 200, 0)),
        ]
        .join("\n");
        let days = fold_transcript(&text);
        let model = model_charge(&days, "deepseek-flash");
        assert_eq!(model.peak.miss, 100, "the morning charge is a peak one");
        assert_eq!(model.off.miss, 200, "the evening charge is an off-peak one");
        assert_eq!(model.total(), 300, "and both are still counted");
    }

    /// A model this build has no rate for costs nothing and is named, rather than being priced off
    /// another model's card and reported as if the vendor had said so.
    #[test]
    fn an_unpriced_model_is_named_rather_than_guessed_at() {
        let charge = Charge { hit: 0, miss: 1_000_000, write: 0, output: 0 };
        let q = quote_of(&[("some-new-model", DayCharge { off: charge, ..Default::default() })]);
        assert_eq!(q.total, 0.0);
        assert_eq!(q.unpriced.len(), 1);
        assert!(q.unpriced.contains("some-new-model"));
    }

    /// A transcript naming no model at all still shows up as something a person can read.
    #[test]
    fn an_unlabelled_model_is_still_named() {
        let charge = Charge { hit: 0, miss: 1_000_000, write: 0, output: 0 };
        let q = quote_of(&[("", DayCharge { off: charge, ..Default::default() })]);
        assert_eq!(q.unpriced.len(), 1);
        assert!(q.unpriced.contains(UNLABELLED_MODEL));
    }

    /// The Pro card is a different row of the same table, not a multiple of the Flash one.
    #[test]
    fn the_pro_card_is_priced_off_its_own_row() {
        let charge = Charge { hit: 0, miss: 0, write: 0, output: 1_000_000 };
        let q = quote_of(&[("deepseek-v4-pro", DayCharge { off: charge, ..Default::default() })]);
        assert!((q.total - 13.5).abs() < 1e-9, "total was {}", q.total);
    }

    /// The vendor's own retired aliases are billed as Flash, so they must be priced and not dropped.
    #[test]
    fn the_retired_flash_aliases_share_the_flash_card() {
        assert_eq!(rate_card("deepseek-flash"), rate_card("deepseek-v4-flash"));
        assert!(rate_card("deepseek-v4-flash-vision-exp").is_some());
        assert!(rate_card("deepseek-v4-pro").is_some());
        assert!(rate_card("gpt-5").is_none());
        assert!(rate_card("").is_none());
    }

    /// Only the parts are ever summed into the total, so a caller cannot report a figure that
    /// disagrees with the rows printed under it.
    #[test]
    fn the_total_is_the_sum_of_the_parts() {
        let charge = Charge { hit: 7, miss: 11, write: 13, output: 17 };
        let q = quote_of(&[("deepseek-flash", DayCharge { off: charge, ..Default::default() })]);
        assert!((q.total - (q.hit_cost + q.miss_cost + q.output_cost)).abs() < 1e-12);
        // A cache write rides with the misses: the vendor bills that token once, at the miss rate.
        assert_eq!(q.miss_tokens, 24);
    }

    /// The estimate the card prints is built from the quote, so the note and the rows agree.
    #[test]
    fn the_cost_estimate_carries_the_quote_it_was_built_from() {
        let charge = Charge { hit: 1_000_000, miss: 0, write: 0, output: 0 };
        let q = quote_of(&[("deepseek-flash", DayCharge { off: charge, ..Default::default() })]);
        let estimate = cost_estimate(&q);
        assert_eq!(estimate.currency, "CNY");
        assert!(estimate.estimated, "a rate-card figure is never a bill");
        assert_eq!(estimate.parts.len(), 3);
        assert_eq!(estimate.parts[0].tokens, 1_000_000);
        assert!((estimate.parts[0].cost - 0.02).abs() < 1e-9);
        assert!(estimate.unpriced.is_empty());
    }

    // ---- activity: the last turn edge is the state ----

    /// One real Zstandard frame per line, the way the harness writes them. Compressing for real
    /// rather than stubbing the decoder out is the point: the frame walk in `read_state` is half of
    /// what is being tested, and a fixture that handed back plain text would skip it.
    fn framed(rows: &[serde_json::Value]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for row in rows {
            out.extend(zstd::encode_all(line(row.clone()).as_bytes(), 3).unwrap());
        }
        out
    }

    fn log_at(name: &str, rows: &[serde_json::Value]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("codenotch-dsh-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        std::fs::write(&path, framed(rows)).unwrap();
        path
    }

    /// A harness log's opening, which is where the title and the working directory are written.
    fn opening(cwd: &str, title: &str) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"type": "session", "seq": 0, "time": 1_700_000_000_000u64, "cwd": cwd}),
            serde_json::json!({"type": "session/title", "seq": 1, "time": 1_700_000_001_000u64, "data": {"title": title}}),
        ]
    }

    fn turn_start(turn: i64, at: u64) -> serde_json::Value {
        serde_json::json!({"type": "turn/start", "seq": 10, "time": at, "data": {"turn": turn}})
    }

    fn turn_end(turn: i64, at: u64) -> serde_json::Value {
        turn_end_because(turn, at, "completed")
    }

    /// The same record with the harness's own word for why the turn stopped. `TurnEndReason` is
    /// merge-extensible, so anything at all can arrive here.
    fn turn_end_because(turn: i64, at: u64, kind: &str) -> serde_json::Value {
        serde_json::json!({"type": "turn/end", "seq": 20, "time": at, "data": {"turn": turn, "reason": {"kind": kind}}})
    }

    #[test]
    fn a_log_that_ends_on_turn_end_is_not_working() {
        let mut rows = opening("D:\\work", "A finished session");
        rows.push(turn_start(1, 1_700_000_010_000));
        rows.push(serde_json::json!({"type": "tool/call", "seq": 11, "time": 1_700_000_011_000u64, "data": {"turn": 1, "step": 1, "name": "pwsh"}}));
        rows.push(serde_json::json!({"type": "tool/result", "seq": 12, "time": 1_700_000_012_000u64, "data": {"turn": 1, "step": 1}}));
        rows.push(turn_end(1, 1_700_000_020_000));
        let state = read_state(&log_at("finished.v3.jsonl.zstd", &rows)).unwrap();
        assert_eq!(state.edge, Some(Edge::TurnClosed));
        assert_eq!(state.at, 1_700_000_020_000);
        assert!(!state.working(), "a closed turn is the end of the work, not the middle");
        assert!(!state.waiting());
        assert!(state.completed(), "and `completed` is the harness saying the work was finished");
        assert_eq!(state.outcome.as_deref(), Some("completed"));
    }

    /// The five edges are not the same question as "did this finish". An abort, a failure and a
    /// token ceiling all close a turn, and drawing a green ring for any of them would say the
    /// opposite of what happened — so only the harness's own `completed` counts.
    #[test]
    fn only_a_completed_turn_counts_as_finished() {
        for kind in ["aborted", "error", "max-tokens", "blocked", "crash-orphaned"] {
            let mut rows = opening("D:\\work", "Cut short");
            rows.push(turn_start(1, 1_700_000_010_000));
            rows.push(turn_end_because(1, 1_700_000_020_000, kind));
            let state = read_state(&log_at("cut.v3.jsonl.zstd", &rows)).unwrap();
            assert_eq!(state.edge, Some(Edge::TurnClosed), "{kind} still closes the turn");
            assert!(!state.working(), "{kind} is over");
            assert!(!state.completed(), "{kind} is not a piece of work finishing");
            assert_eq!(state.outcome.as_deref(), Some(kind));
        }
    }

    /// A `turn/end` with no `reason` at all — an older log, or a plugin that replaced the map — is
    /// closed and unexplained, which is not the same as completed.
    #[test]
    fn a_turn_end_with_no_reason_is_closed_but_not_finished() {
        let mut rows = opening("D:\\work", "Unexplained");
        rows.push(turn_start(1, 1_700_000_010_000));
        rows.push(serde_json::json!({"type": "turn/end", "seq": 20, "time": 1_700_000_020_000u64, "data": {"turn": 1}}));
        let state = read_state(&log_at("bareend.v3.jsonl.zstd", &rows)).unwrap();
        assert_eq!(state.edge, Some(Edge::TurnClosed));
        assert_eq!(state.outcome, None);
        assert!(!state.completed());
    }

    /// The reason belongs to the same record as the edge it explains. A `turn/end` that closed an
    /// earlier turn must not lend its reason to a later `turn/start`.
    #[test]
    fn the_reason_belongs_to_the_edge_it_came_with() {
        let mut rows = opening("D:\\work", "Two turns, one finished");
        rows.push(turn_start(1, 1_700_000_010_000));
        rows.push(turn_end_because(1, 1_700_000_020_000, "aborted"));
        rows.push(turn_start(2, 1_700_000_030_000));
        let state = read_state(&log_at("reopened.v3.jsonl.zstd", &rows)).unwrap();
        assert_eq!(state.edge, Some(Edge::TurnOpen));
        assert_eq!(state.outcome, None, "the open turn explains nothing");
        assert!(!state.completed());
    }

    #[test]
    fn a_turn_that_started_and_never_ended_is_working() {
        let mut rows = opening("D:\\work", "Running right now");
        rows.push(turn_start(1, 1_700_000_010_000));
        rows.push(serde_json::json!({"type": "step/start", "seq": 11, "time": 1_700_000_010_500u64, "data": {"turn": 1, "step": 1}}));
        rows.push(serde_json::json!({"type": "tool/call", "seq": 12, "time": 1_700_000_011_000u64, "data": {"turn": 1, "step": 1, "name": "pwsh"}}));
        let state = read_state(&log_at("open.v3.jsonl.zstd", &rows)).unwrap();
        assert_eq!(state.edge, Some(Edge::TurnOpen));
        assert_eq!(state.at, 1_700_000_010_000);
        assert!(state.working());
        assert!(!state.waiting());
    }

    /// The pair is what makes this state rather than a guess: an `asked` on its own is a question
    /// still on screen, and the `decided` that follows it puts the session back to work.
    #[test]
    fn an_unanswered_approval_is_waiting_and_an_answered_one_is_not() {
        let base = {
            let mut rows = opening("D:\\work", "Waiting on you");
            rows.push(turn_start(1, 1_700_000_010_000));
            rows.push(serde_json::json!({"type": "tool/call", "seq": 11, "time": 1_700_000_011_000u64, "data": {"turn": 1, "step": 1, "name": "pwsh"}}));
            rows
        };
        let mut asked = base.clone();
        asked.push(serde_json::json!({"type": "approval/asked", "seq": 12, "time": 1_700_000_012_000u64, "data": {"id": "a-1", "toolName": "pwsh"}}));
        let waiting = read_state(&log_at("asked.v3.jsonl.zstd", &asked)).unwrap();
        assert!(waiting.waiting(), "an unanswered question is the one state that wants something");
        assert!(!waiting.working(), "waiting outranks working rather than joining it");
        assert_eq!(waiting.at, 1_700_000_012_000);

        let mut decided = asked;
        decided.push(serde_json::json!({"type": "approval/decided", "seq": 13, "time": 1_700_000_013_000u64, "data": {"id": "a-1", "decision": "allow"}}));
        let settled = read_state(&log_at("decided.v3.jsonl.zstd", &decided)).unwrap();
        assert!(settled.working());
        assert!(!settled.waiting(), "the answer was given; there is nothing to wait for now");
    }

    /// A second turn opening after the first closed is the ordinary case of a session you came back
    /// to, and the outward scan has to stop on the newer `turn/start` rather than the older pair.
    #[test]
    fn the_second_turn_is_the_one_that_decides() {
        let mut rows = opening("D:\\work", "Two turns");
        rows.push(turn_start(1, 1_700_000_010_000));
        rows.push(turn_end(1, 1_700_000_020_000));
        rows.push(serde_json::json!({"type": "user/message", "seq": 21, "time": 1_700_000_030_000u64, "data": {"role": "user"}}));
        rows.push(turn_start(2, 1_700_000_031_000));
        rows.push(serde_json::json!({"type": "step/start", "seq": 22, "time": 1_700_000_031_500u64, "data": {"turn": 2, "step": 1}}));
        let state = read_state(&log_at("twoturns.v3.jsonl.zstd", &rows)).unwrap();
        assert_eq!(state.edge, Some(Edge::TurnOpen));
        assert_eq!(state.at, 1_700_000_031_000);
        assert!(state.working());
    }

    #[test]
    fn the_header_and_the_title_are_read_from_the_opening_frames() {
        let mut rows = opening("D:\\OB\\DAV", "统计 Obsidian 知识库笔记字数");
        rows.push(turn_start(1, 1_700_000_010_000));
        rows.push(turn_end(1, 1_700_000_020_000));
        let state = read_state(&log_at("named.v3.jsonl.zstd", &rows)).unwrap();
        assert_eq!(state.title, "统计 Obsidian 知识库笔记字数");
        assert_eq!(state.cwd, "D:\\OB\\DAV");
    }

    /// A log whose last frame is half-written — the normal state of one being appended to right now
    /// — must still answer from the frames that did land.
    #[test]
    fn a_torn_trailing_frame_does_not_hide_the_turn_before_it() {
        let mut rows = opening("D:\\work", "Torn tail");
        rows.push(turn_start(1, 1_700_000_010_000));
        let mut bytes = framed(&rows);
        bytes.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x11, 0x22]); // a frame that never finished
        let dir = std::env::temp_dir().join(format!("codenotch-dsh-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("torn.v3.jsonl.zstd");
        std::fs::write(&path, &bytes).unwrap();
        let state = read_state(&path).unwrap();
        assert_eq!(state.edge, Some(Edge::TurnOpen));
    }

    #[test]
    fn a_log_with_no_turn_at_all_is_neither_working_nor_waiting() {
        let state = read_state(&log_at("bare.v3.jsonl.zstd", &opening("D:\\work", "Set up only"))).unwrap();
        assert_eq!(state.edge, None);
        assert!(!state.working());
        assert!(!state.waiting());
        assert_eq!(state.at, 0);
    }
}
