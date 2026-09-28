//! WorkBuddy usage adapter, ported from Token Monitor's WorkBuddy provider
//! (`src/shared/providers/workbuddy/limits.js` and `src/electron/providers/workbuddy/localAuth.js`)
//! and from the provider notes in `docs/providers/workbuddy.md`.
//!
//! WorkBuddy has two independent data planes, and this cell carries both of them:
//!
//!   - **The balance** — the Credits still held, read from the app's own session (below). This is
//!     the half the cell is named for, and the half a sealed credential takes away.
//!   - **Local token usage** — totalled from the app's own session transcripts by
//!     `workbuddy_tokens.rs`, which needs no credential at all. Token Monitor carries the same
//!     plane through `tokscale`. A machine whose credential is sealed still gets a number here
//!     instead of an empty cell, and no machine loses the history it already wrote.
//!
//! The balance leads the cell when it can be read; the token windows lead it when it cannot. Both
//! are in one snapshot so the card, the ring and the tray menu never have to choose between them.
//!
//! Data path — the same bargain the other providers strike: borrow the app's own session.
//!
//!   1. Credential. The installed WorkBuddy desktop app writes its session to
//!      `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`, falling back to
//!      `%APPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`. Codenotch only ever
//!      reads it: nothing is written back, the token is never refreshed by us, and it never reaches
//!      a log line, an event payload or the UI.
//!   2. Endpoint. `POST https://copilot.tencent.com/v2/billing/meter/get-user-resource` for a
//!      personal account, or `/v2/billing/meter/get-enterprise-user-usage` when the session carries
//!      an enterprise id. The reply states the Credits each resource package still holds; that
//!      aggregate is the ring.
//!
//! The rules below are the ones that have to survive a refactor:
//!
//!   - **The first directory with canonical state decides.** Once `%LOCALAPPDATA%` holds the file or
//!     a `.logged-out` marker, the reader does not fall back to `%APPDATA%`, so a stale roaming copy
//!     cannot revive a session the user ended.
//!   - **Only the canonical filename is trusted**, and only as a regular file: a symlink, a
//!     directory or anything over 1 MB is refused outright rather than followed.
//!   - **Status 3 packages are not part of the spendable balance.** The request asks for them
//!     because the official client does, but only Status 0 rows are aggregated.
//!   - **Failing closed beats a plausible number.** If an active package carries unusable quota
//!     data the whole reading is refused; a partial aggregate would look right while silently
//!     omitting a package the user can still spend.
//!   - **An encrypted credential is not a signed-out app.** WorkBuddy 5.6.0 and later seal each
//!     credential field with a key their own runtime holds, so `auth.accessToken` arrives as
//!     `{$wbEncrypted: 1, …}`. Codenotch cannot open that envelope, and telling the user to sign in
//!     again sends them to a screen that cannot change the outcome — so that state is reported as
//!     its own thing, never as `needsAuth`.
//!
//! Reply shapes accepted, personal (`Accounts` is the array that matters):
//! ```text
//! { "code": 0, "data": { "Response": { "Data": { "Accounts": [
//!     { "Status": 0, "AccountId": "…",
//!       "CycleCapacitySizePrecise": 200000, "CycleCapacityRemainPrecise": 173000,
//!       "CycleCapacityUsedPrecise": 27000 } ] } } } }
//! ```
//! and enterprise: `{ "data": { "limitNum": 2000, "credit": 780, "cycleResetTime": "…" } }`, where
//! `limitNum: -1` means the plan is unmetered.

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::workbuddy_tokens::{self, TotalsCache};
use crate::AppState;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const ENDPOINT: &str = "https://copilot.tencent.com";
const PERSONAL_PATH: &str = "/v2/billing/meter/get-user-resource";
const ENTERPRISE_PATH: &str = "/v2/billing/meter/get-enterprise-user-usage";
/// The product the official client asks for, so the server selects the same packages it does.
const PRODUCT_CODE: &str = "p_tcaca";
/// The official client's window: it wants every package that expires inside the next 101 years.
const PERSONAL_RANGE_MS: u64 = 101 * 365 * 24 * 60 * 60 * 1000;
const POLL_SECS: u64 = 300;
/// The WorkBuddy desktop app is looked for again this often while it is not installed.
const ABSENT_POLL_SECS: u64 = 600;
const FETCH_TIMEOUT_SECS: u64 = 12;

const AUTH_FILE_NAME: &str = "workbuddy-desktop.info";
const LOGOUT_MARKER_SUFFIX: &str = ".logged-out";
const AUTH_FILE_MAX_BYTES: u64 = 1024 * 1024;
/// A session this close to expiry is treated as expired, so a request is not started on a token
/// that will be rejected moments later.
const EXPIRY_SKEW_MS: u64 = 30 * 1000;
/// The marker the app puts on a field it sealed with its own runtime's key.
const ENCRYPTED_FIELD_MARKER: &str = "$wbEncrypted";
/// What the card says when a credential the user pasted is refused.
///
/// One string rather than two, because the card has one line for it and both routes into this state —
/// the billing request answering 401, and the balance simply being missing — call for the same advice:
/// a fresh token. Sending someone whose paste was refused to the app's sign-in screen would point at
/// a screen that cannot help while a stale credential file sits in the way.
const PASTED_REFUSED: &str = "WorkBuddy refused the pasted credential — paste a fresh one in Settings";

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("workbuddy.json")
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

/// The per-file token totals, so a restart does not have to fold the whole transcript history
/// again. It holds no conversation text and no credential — a size, an mtime and a count per day.
fn tokens_cache_path() -> PathBuf {
    crate::config::config_path().with_file_name("workbuddy-tokens-cache.json")
}

fn load_tokens_cache() -> TotalsCache {
    std::fs::read_to_string(tokens_cache_path())
        .ok()
        .and_then(|t| serde_json::from_str::<TotalsCache>(&t).ok())
        .unwrap_or_default()
}

fn save_tokens_cache(cache: &TotalsCache) {
    if let Ok(t) = serde_json::to_string(cache) {
        let _ = std::fs::write(tokens_cache_path(), t);
    }
}

// ---------------- where the session lives ----------------

/// Windows: `%LOCALAPPDATA%` first, then `%APPDATA%`. The order is the whole point: the app writes
/// to the first one, and a legacy roaming copy must never outrank it.
fn auth_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in [dirs::data_local_dir(), dirs::data_dir()] {
        if let Some(root) = root {
            let dir = root.join("CodeBuddyExtension").join("Data").join("Public").join("auth");
            if !out.contains(&dir) {
                out.push(dir);
            }
        }
    }
    out
}

fn auth_file_in(dir: &Path) -> PathBuf {
    dir.join(AUTH_FILE_NAME)
}

fn logout_marker_in(dir: &Path) -> PathBuf {
    dir.join(format!("{AUTH_FILE_NAME}{LOGOUT_MARKER_SUFFIX}"))
}

// ---------------- a credential the user pasted in ----------------

/// The credential file lives beside the config. It exists for the one case the reader cannot solve on
/// its own: WorkBuddy 5.6.0 and later seal `auth.accessToken` with a key only their own runtime
/// holds, so the installed app's session is unreadable to everybody else. Reading the balance then
/// leaves exactly two options — ask the person for their token, or attack the envelope — and only the
/// first is this program's business.
///
/// Its own file rather than a field in `config.json`: that document is rewritten wholesale by the
/// settings window and handed back to the page, and a credential should not be anywhere near it.
fn credential_path() -> PathBuf {
    crate::config::config_path().with_file_name("workbuddy-credential.json")
}

/// What the user pasted. Only the token is required — everything else is used when it is there. The
/// enterprise id is the one field that changes the request, since it selects the endpoint.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ManualCredential {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub enterprise_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub account_type: String,
}

/// The pasted credential, if one is stored and usable. A file with a blank token counts as none, so
/// "saved" and "will be sent" can never disagree.
pub fn read_credential() -> Option<ManualCredential> {
    let text = std::fs::read_to_string(credential_path()).ok()?;
    let credential = serde_json::from_str::<ManualCredential>(&text).ok()?;
    if credential.access_token.trim().is_empty() {
        return None;
    }
    Some(credential)
}

pub fn credential_saved() -> bool {
    read_credential().is_some()
}

/// The token as it should be stored, or `None` when there is nothing to store.
///
/// A pasted token usually arrives the way it was copied out of a browser's network panel, scheme and
/// all. Cleaning it here rather than refusing it matters: refusing would send the user back to copy
/// the same string again with no idea which part was wrong.
fn normalized_token(raw: &str) -> Option<String> {
    let token = raw.trim().trim_start_matches("Bearer ").trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Writes the credential, and nothing else. Returns the state that was actually stored so the caller
/// reports what happened rather than what it asked for.
pub fn save_credential(access_token: &str, enterprise_id: &str, user_id: &str) -> Result<(), String> {
    let Some(token) = normalized_token(access_token) else {
        return Err("the credential is empty".into());
    };
    let credential = ManualCredential {
        access_token: token,
        enterprise_id: enterprise_id.trim().to_string(),
        user_id: user_id.trim().to_string(),
        account_type: if enterprise_id.trim().is_empty() { "personal".into() } else { "enterprise".into() },
    };
    let text = serde_json::to_string_pretty(&credential).map_err(|e| e.to_string())?;
    std::fs::write(credential_path(), text).map_err(|e| e.to_string())?;
    request_refresh();
    Ok(())
}

/// Removes the file. A missing file is success: the point is the state afterwards, not who made it.
pub fn forget_credential() -> Result<(), String> {
    match std::fs::remove_file(credential_path()) {
        Ok(()) => {
            request_refresh();
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            request_refresh();
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The same session shape the app's own file produces, built from what was pasted.
///
/// `expires_at` is zero — "no stated expiry" — because a pasted token says nothing about when it
/// lapses and the sealed file cannot be consulted for it. The request itself is then the test: a
/// token that has aged out answers 401 and the card asks for a fresh one.
fn session_from_credential(credential: &ManualCredential) -> Session {
    Session {
        token: normalized_token(&credential.access_token).unwrap_or_default(),
        user_id: credential.user_id.trim().to_string(),
        enterprise_id: credential.enterprise_id.trim().to_string(),
        department_info: String::new(),
        domain: String::new(),
        account_type: if credential.account_type.trim().is_empty() {
            "personal".to_string()
        } else {
            credential.account_type.trim().to_string()
        },
        expires_at: 0,
    }
}

/// Whether the installed app left canonical state here at all, or — since the transcripts outlive a
/// sealed credential — whether there is any session history to total. Used to tell "WorkBuddy is not
/// on this machine" (no cell at all) from "WorkBuddy is here and signed out" (a cell asking to sign
/// in).
///
/// A pasted credential counts as present on its own: someone who pasted a token wants the cell even
/// if the desktop app is not installed here — the token works against the same API from anywhere.
pub fn present() -> bool {
    credential_saved()
        || auth_dirs().iter().any(|d| auth_file_in(d).is_file() || logout_marker_in(d).exists())
        || workbuddy_tokens::present()
}

// ---------------- reading the session ----------------

/// Why a session could not be used. The distinction is the point: only `Encrypted` changes what the
/// user can do about it, and only `Absent`/`Malformed`/`Incomplete` are answered with "sign in".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadReason {
    /// No canonical file, or the app left its logout marker.
    Absent,
    /// The canonical file exists but is not a readable session document.
    Malformed,
    /// A readable document without a usable access token or account id.
    Incomplete,
    /// The app sealed its credential fields; we cannot decrypt them.
    Encrypted,
    /// A usable session whose expiry has passed.
    Expired,
}

impl ReadReason {
    fn note(self) -> &'static str {
        match self {
            ReadReason::Absent => "WorkBuddy is not signed in — open the app and sign in once",
            ReadReason::Malformed => "The WorkBuddy session file is not readable JSON",
            ReadReason::Incomplete => "The WorkBuddy session holds no usable token or account id",
            ReadReason::Encrypted => {
                "WorkBuddy sealed this credential with its own key (5.6.0+), so Codenotch cannot read it. \
                 Signing in again will not change that."
            }
            ReadReason::Expired => "The WorkBuddy session has expired — sign in again in the app",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    token: String,
    user_id: String,
    enterprise_id: String,
    department_info: String,
    domain: String,
    account_type: String,
    /// ms epoch; 0 = the file names no expiry
    expires_at: u64,
}

impl Session {
    fn expired(&self, now: u64) -> bool {
        self.expires_at > 0 && self.expires_at <= now + EXPIRY_SKEW_MS
    }
}

fn text_of(v: Option<&serde_json::Value>) -> String {
    v.and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
}

fn ms_of(v: Option<&serde_json::Value>) -> u64 {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

/// A field the app sealed with a key it holds. We can detect it and nothing more.
fn is_encrypted_field(value: Option<&serde_json::Value>) -> bool {
    value
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key(ENCRYPTED_FIELD_MARKER))
        .unwrap_or(false)
}

/// Session-or-reason, the single parse both the reader and `doctor` go through. Kept pure so every
/// branch is testable without a file, a clock or a network.
pub fn inspect_session(value: &serde_json::Value, _now: u64) -> (Option<Session>, Option<ReadReason>) {
    let Some(root) = value.as_object() else {
        return (None, Some(ReadReason::Malformed));
    };
    let auth = root.get("auth").and_then(|v| v.as_object());
    let account = root.get("account").and_then(|v| v.as_object());
    // Only the access token gates the read: it is the one field the billing request consumes, so a
    // sealed refresh token must not mark an otherwise usable session as unreadable.
    if is_encrypted_field(auth.and_then(|a| a.get("accessToken"))) {
        return (None, Some(ReadReason::Encrypted));
    }
    let token = text_of(auth.and_then(|a| a.get("accessToken")));
    let user_id = text_of(account.and_then(|a| a.get("uid")));
    if token.is_empty() || user_id.is_empty() {
        return (None, Some(ReadReason::Incomplete));
    }
    let account_type = {
        let explicit = text_of(account.and_then(|a| a.get("accountType")));
        if explicit.is_empty() {
            let alt = text_of(account.and_then(|a| a.get("type")));
            if alt.is_empty() { "personal".to_string() } else { alt }
        } else {
            explicit
        }
    };
    let session = Session {
        token,
        user_id,
        enterprise_id: text_of(account.and_then(|a| a.get("enterpriseId"))),
        department_info: text_of(account.and_then(|a| a.get("departmentFullName"))),
        domain: text_of(auth.and_then(|a| a.get("domain"))),
        account_type,
        expires_at: ms_of(auth.and_then(|a| a.get("expiresAt"))),
    };
    (Some(session), None)
}

/// Reads a regular file, and only a regular file: `symlink_metadata` reports the link itself, so a
/// symlink is rejected here rather than followed to somewhere the app never wrote.
fn read_regular_file(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > AUTH_FILE_MAX_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The first directory holding canonical state — the file or the logout marker — decides.
pub fn read_session(now: u64) -> (Option<Session>, Option<ReadReason>) {
    for dir in auth_dirs() {
        let file = auth_file_in(&dir);
        if !file.is_file() && !logout_marker_in(&dir).exists() {
            continue;
        }
        if logout_marker_in(&dir).exists() {
            return (None, Some(ReadReason::Absent));
        }
        let Some(text) = read_regular_file(&file) else {
            // A canonical file that cannot be read is not the same as no file: only a path that is
            // genuinely gone counts as absent.
            return if file.exists() {
                (None, Some(ReadReason::Malformed))
            } else {
                (None, Some(ReadReason::Absent))
            };
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return (None, Some(ReadReason::Malformed));
        };
        return inspect_session(&value, now);
    }
    (None, Some(ReadReason::Absent))
}

/// For doctor: a report that never contains a credential value.
pub fn probe() -> String {
    // Checked before the directories: a pasted credential works whether or not the desktop app is
    // installed here, so the report has to lead with which credential is actually in use.
    if let Some(credential) = read_credential() {
        return format!(
            "WorkBuddy: pasted credential in use (token {} chars, {}{})",
            credential.access_token.trim().len(),
            if credential.enterprise_id.trim().is_empty() { "personal" } else { "enterprise" },
            if credential.user_id.trim().is_empty() { "" } else { ", user id set" }
        );
    }
    let dirs = auth_dirs();
    let Some(first) = dirs.first() else {
        return "WorkBuddy: no home directory to look under".into();
    };
    if !present() {
        return format!(
            "WorkBuddy: {} not found (the desktop app is not installed, or has never signed in)",
            auth_file_in(first).display()
        );
    }
    let (session, reason) = read_session(now_ms());
    match (session, reason) {
        (Some(s), _) => format!(
            "WorkBuddy: session borrowed (token {} chars, {}, {}, account {}{})",
            s.token.len(),
            if s.expires_at == 0 { "no stated expiry" } else if s.expired(now_ms()) { "expired" } else { "live" },
            s.account_type,
            if s.user_id.is_empty() { "?" } else { "ok" },
            if s.enterprise_id.is_empty() { String::new() } else { ", enterprise".to_string() }
        ),
        (None, Some(r)) => format!("WorkBuddy: session unusable ({r:?}) — {}", r.note()),
        (None, None) => "WorkBuddy: session unusable".into(),
    }
}

// ---------------- parsing ----------------

fn pick_value<'a>(source: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    let obj = source.as_object()?;
    for key in keys {
        if let Some(v) = obj.get(*key) {
            let empty_string = v.as_str().map(|s| s.is_empty()).unwrap_or(false);
            if !v.is_null() && !empty_string {
                return Some(v);
            }
        }
    }
    None
}

fn number_or_null(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() { None } else { t.parse::<f64>().ok() }
        }
        _ => None,
    }
}

fn pick_array<'a>(source: &'a serde_json::Value, paths: &[&[&str]]) -> Option<&'a Vec<serde_json::Value>> {
    for path in paths {
        let mut current = source;
        let mut found = true;
        for key in path.iter() {
            match current.get(*key) {
                Some(next) => current = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(array) = current.as_array() {
                return Some(array);
            }
        }
    }
    None
}

/// What one billing reply says about the spendable balance.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// A metered balance: the aggregate window, ready for the card.
    Metered { used: f64, limit: f64, remaining: f64, resets_at: Option<u64> },
    /// Nothing is metered — an unmetered enterprise plan, or an account with no active package.
    Unmetered(String),
}

fn personal_accounts(body: &serde_json::Value) -> Result<&Vec<serde_json::Value>, String> {
    pick_array(
        body,
        &[
            &["data", "Response", "Data", "Accounts"],
            &["data", "data", "Response", "Data", "Accounts"],
            &["Response", "Data", "Accounts"],
            &["data", "response", "data", "accounts"],
            &["response", "data", "accounts"],
        ],
    )
    .ok_or_else(|| "WorkBuddy personal billing response has no Accounts array".to_string())
}

/// Personal accounts aggregate every active (Status 0) package. A package whose numbers cannot be
/// read fails the whole reading rather than being quietly left out.
pub fn parse_personal_usage(body: &serde_json::Value) -> Result<Reading, String> {
    let resources = personal_accounts(body)?;
    let mut used = 0.0f64;
    let mut limit = 0.0f64;
    let mut remaining = 0.0f64;
    let mut valid = 0usize;
    let mut candidates = 0usize;

    for resource in resources {
        if !resource.is_object() {
            candidates += 1;
            continue;
        }
        let status = number_or_null(pick_value(resource, &["Status", "status"]));
        // The official client requests Status 3 packages for its wider UI, but those historical
        // rows are not part of the currently spendable balance.
        if let Some(s) = status {
            if s != 0.0 {
                continue;
            }
        }
        candidates += 1;
        let total = number_or_null(pick_value(resource, &["CycleCapacitySizePrecise", "cycleCapacitySizePrecise"]));
        let left = number_or_null(pick_value(resource, &["CycleCapacityRemainPrecise", "cycleCapacityRemainPrecise"]));
        let (Some(total), Some(left)) = (total, left) else { continue };
        if total < 0.0 || left < 0.0 {
            continue;
        }
        let safe_remaining = left.min(total);
        let reported = number_or_null(pick_value(resource, &["CycleCapacityUsedPrecise", "cycleCapacityUsedPrecise"]));
        let safe_used = match reported {
            Some(u) => u.clamp(0.0, total),
            None => (total - safe_remaining).max(0.0),
        };
        limit += total;
        remaining += safe_remaining;
        used += safe_used;
        valid += 1;
    }

    if candidates > valid {
        return Err("WorkBuddy billing response contains unusable active resource packages".into());
    }
    if valid == 0 || limit <= 0.0 {
        return Ok(Reading::Unmetered("No active WorkBuddy Credit package on this account yet".into()));
    }
    let used = used.clamp(0.0, limit);
    Ok(Reading::Metered { used, limit, remaining: remaining.min(limit), resets_at: None })
}

fn unwrap_enterprise_usage(body: &serde_json::Value) -> Option<&serde_json::Value> {
    for candidate in [body.get("data").and_then(|d| d.get("data")), body.get("data"), body.get("Data")] {
        if let Some(v) = candidate {
            if v.is_object() {
                return Some(v);
            }
        }
    }
    if body.is_object() { Some(body) } else { None }
}

/// Enterprise replies state a limit and a spend, and a reset instant. `limitNum: -1` is the
/// vendor's way of saying the plan is unlimited.
pub fn parse_enterprise_usage(body: &serde_json::Value) -> Result<Reading, String> {
    let usage = unwrap_enterprise_usage(body)
        .ok_or_else(|| "WorkBuddy enterprise billing response is not an object".to_string())?;
    let limit = number_or_null(pick_value(usage, &["limitNum", "limit_num"]))
        .ok_or_else(|| "WorkBuddy enterprise billing response has no limitNum".to_string())?;
    if limit < 0.0 && limit != -1.0 {
        return Err("WorkBuddy enterprise billing response has an invalid limitNum".into());
    }
    let reported_used = number_or_null(pick_value(usage, &["credit", "used", "usedNum", "used_num"]))
        .ok_or_else(|| "WorkBuddy enterprise billing response has no usage value".to_string())?;
    if reported_used < 0.0 {
        return Err("WorkBuddy enterprise billing response has an invalid usage value".into());
    }
    let resets_at = parse_instant(pick_value(usage, &["cycleResetTime", "cycle_reset_time"]));
    if limit == -1.0 {
        return Ok(Reading::Unmetered("Unmetered WorkBuddy enterprise plan (unlimited Credits)".into()));
    }
    let limit = limit.max(0.0);
    if limit <= 0.0 {
        return Ok(Reading::Unmetered("No WorkBuddy Credit allowance is published for this plan".into()));
    }
    let used = reported_used.min(limit);
    Ok(Reading::Metered { used, limit, remaining: (limit - used).max(0.0), resets_at })
}

/// A millisecond epoch or an ISO-8601 instant, as ms epoch. WorkBuddy's numeric timestamps are
/// milliseconds, unlike the Unix-seconds convention a few other providers use.
fn parse_instant(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    match value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            if t.chars().all(|c| c.is_ascii_digit()) {
                return t.parse::<u64>().ok();
            }
            chrono::DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|d| d.timestamp_millis().max(0) as u64)
        }
        _ => None,
    }
}

/// `2026-09-27 00:18:00`, the format the billing request states its package window in.
fn format_local(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

// ---------------- the request ----------------

enum FetchErr {
    NeedsAuth,
    Unavailable(String),
}

fn fetch_once(session: &Session) -> Result<serde_json::Value, FetchErr> {
    let enterprise = !session.enterprise_id.is_empty();
    let (url, body) = if enterprise {
        (format!("{ENDPOINT}{ENTERPRISE_PATH}"), "{}".to_string())
    } else {
        let now = now_ms();
        let payload = serde_json::json!({
            "PageNumber": 1,
            "PageSize": 100,
            "ProductCode": PRODUCT_CODE,
            // Match the server-side package selection the desktop client uses. The aggregate still
            // excludes Status 3 rows.
            "Status": [0, 3],
            "PackageEndTimeRangeBegin": format_local(now),
            "PackageEndTimeRangeEnd": format_local(now + PERSONAL_RANGE_MS),
        });
        (format!("{ENDPOINT}{PERSONAL_PATH}"), payload.to_string())
    };

    let mut req = ureq::post(&url)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {}", session.token))
        .set("X-User-Id", &session.user_id);
    if enterprise {
        req = req.set("X-Enterprise-Id", &session.enterprise_id).set("X-Tenant-Id", &session.enterprise_id);
    }
    if !session.domain.is_empty() {
        req = req.set("X-Domain", &session.domain);
    }
    if !session.department_info.is_empty() {
        req = req.set("X-Department-Info", &session.department_info);
    }

    // ureq never follows a redirect, which is the property that matters here: the app's token is
    // allowed to travel to exactly one URL and nowhere a redirect might point it.
    match req.send_string(&body) {
        Ok(r) => {
            let status = r.status();
            if (300..400).contains(&status) {
                return Err(FetchErr::Unavailable(format!("HTTP {status} redirect refused")));
            }
            let value: serde_json::Value = r
                .into_json()
                .map_err(|e| FetchErr::Unavailable(format!("parse: {e}")))?;
            if let Some(code) = number_or_null(value.get("code")) {
                if code != 0.0 && code != 200.0 {
                    return Err(if code == 401.0 || code == 403.0 {
                        FetchErr::NeedsAuth
                    } else {
                        FetchErr::Unavailable(format!("WorkBuddy billing application code {code}"))
                    });
                }
            }
            Ok(value)
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => Err(FetchErr::NeedsAuth),
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Unavailable(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Unavailable(format!("{e}"))),
    }
}

/// The one window the ring draws. A personal account names no reset: each package expires on its
/// own day, so there is no single instant to print and none is invented.
///
/// The absolute remainder rides along with the fraction. A ring at 13.5% is the same picture whether
/// the account holds 173,000 credits or 1,730,000, and "how much is left" is the question the cell
/// exists to answer — so the number the vendor did publish is carried through instead of being
/// thrown away by the percentage.
fn window_of(reading: &Reading) -> Option<LimitWindow> {
    match reading {
        Reading::Metered { used, limit, remaining, resets_at } => {
            let fraction = if *limit > 0.0 { (used / limit).clamp(0.0, 1.0) } else { 0.0 };
            Some(LimitWindow {
                id: "credits".into(),
                label: "Credits".into(),
                used: fraction,
                resets_at: *resets_at,
                remaining: Some(*remaining),
                unit: Some("credits".into()),
                ..Default::default()
            })
        }
        Reading::Unmetered(_) => None,
    }
}

/// Why the balance half is missing, in the one line the card has room for. The distinction the read
/// reasons draw still matters here: a sealed credential is not a signed-out app, and answering it
/// with "sign in again" would send the user to a screen that cannot change the outcome.
fn quota_missing_note(reason: Option<ReadReason>) -> String {
    match reason {
        Some(ReadReason::Encrypted) => {
            "Balance sealed by WorkBuddy — showing local token usage".into()
        }
        _ => "No balance available — showing local token usage".into(),
    }
}

/// The reading the cell falls back to when the balance cannot be read but the transcripts can: the
/// local token windows, with a note saying what happened to the other half. `None` when there is no
/// history either, so the original status handling stands unchanged.
fn local_fallback(
    local: &[LimitWindow],
    files: usize,
    now: u64,
    note: String,
) -> Option<UsageSnapshot> {
    if files == 0 {
        return None;
    }
    Some(UsageSnapshot {
        status: "ok".into(),
        windows: local.to_vec(),
        fetched_at: now,
        note,
        backoff_until: 0,
        // Local token counts, not credits: nothing here is priced, so the card is given no money to
        // print. Inventing one from a rate card that does not exist for this product would be worse
        // than the empty space.
        cost: None,
    })
}

fn read_once(prev: &UsageSnapshot, tokens: &mut TotalsCache) -> UsageSnapshot {
    let now = now_ms();
    let mut snap = prev.clone();

    // The local half is read first and unconditionally: it is the one that still answers when the
    // credential is sealed, and a credential that cannot be read must not take it down with it.
    let (days, files) = workbuddy_tokens::scan(tokens, now);
    save_tokens_cache(tokens);
    let local = workbuddy_tokens::windows(&days, now);

    // A credential the user pasted outranks the app's own file, and for the reason the file exists at
    // all: when the app sealed its copy, that file can never answer, and someone who pasted a token
    // did it precisely so this reader would use it.
    let pasted = read_credential();
    let from_paste = pasted.is_some();
    let (session, reason) = match pasted.as_ref() {
        Some(credential) => (Some(session_from_credential(credential)), None),
        None => read_session(now),
    };
    // What to say when the balance is missing. A refused pasted token is not a signed-out app: the
    // remedy is a fresh token in Settings, and pointing at the app's sign-in screen would send the
    // user somewhere that cannot change the outcome.
    let missing = |reason: Option<ReadReason>| -> String {
        if from_paste {
            PASTED_REFUSED.into()
        } else {
            quota_missing_note(reason)
        }
    };

    let Some(session) = session else {
        let reason = reason.unwrap_or(ReadReason::Absent);
        if let Some(s) = local_fallback(&local, files, now, missing(Some(reason))) {
            return s;
        }
        match reason {
            ReadReason::Absent => {
                // Only a machine with no WorkBuddy at all goes quiet; a signed-out app keeps its
                // cell so the reason is visible.
                snap.status = if present() { "needsAuth".into() } else { "absent".into() };
            }
            ReadReason::Encrypted => snap.status = "error".into(),
            ReadReason::Malformed | ReadReason::Incomplete | ReadReason::Expired => {
                snap.status = "needsAuth".into();
            }
        }
        snap.note = reason.note().into();
        return snap;
    };
    if session.expired(now) {
        if let Some(s) = local_fallback(&local, files, now, missing(Some(ReadReason::Expired))) {
            return s;
        }
        snap.status = if snap.windows.is_empty() { "needsAuth" } else { "stale" }.into();
        snap.note = ReadReason::Expired.note().into();
        return snap;
    }

    match fetch_once(&session) {
        Ok(body) => {
            let reading = if session.enterprise_id.is_empty() {
                parse_personal_usage(&body)
            } else {
                parse_enterprise_usage(&body)
            };
            match reading {
                Ok(reading) => {
                    snap.fetched_at = now;
                    match &reading {
                        Reading::Metered { .. } => {
                            snap.status = "ok".into();
                            // The balance leads, because that is what the cell is named for; the
                            // local windows follow it rather than replacing it.
                            let mut windows: Vec<LimitWindow> =
                                window_of(&reading).into_iter().collect();
                            windows.extend(local);
                            snap.windows = windows;
                            snap.note.clear();
                        }
                        Reading::Unmetered(note) => {
                            // Nothing metered is still a good reading: say so rather than drawing a
                            // ring whose percentage would be invented.
                            snap.status = if local.is_empty() { "none" } else { "ok" }.into();
                            snap.windows = local;
                            snap.note = note.clone();
                        }
                    }
                }
                Err(msg) => {
                    if let Some(s) = local_fallback(&local, files, now, msg.clone()) {
                        return s;
                    }
                    // Keep the older reading, marked by its own age, rather than blanking the cell.
                    snap.status = if snap.windows.is_empty() { "error" } else { "stale" }.into();
                    snap.note = msg;
                }
            }
        }
        Err(FetchErr::NeedsAuth) => {
            if let Some(s) = local_fallback(&local, files, now, missing(Some(ReadReason::Expired))) {
                return s;
            }
            snap.status = "needsAuth".into();
            snap.note = if from_paste {
                PASTED_REFUSED.into()
            } else {
                "WorkBuddy rejected the session — sign in again in the app".into()
            };
        }
        Err(FetchErr::Unavailable(msg)) => {
            if let Some(s) = local_fallback(&local, files, now, msg.clone()) {
                return s;
            }
            snap.status = if snap.windows.is_empty() { "error" } else { "stale" }.into();
            snap.note = msg;
        }
    }
    snap
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.workbuddy.lock().unwrap() = snap.clone();
    persist(&snap);
    let _ = app.emit("workbuddy", &snap);
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
        let mut tokens = load_tokens_cache();
        {
            let st = app.state::<AppState>();
            let snap = st.workbuddy.lock().unwrap().clone();
            let _ = app.emit("workbuddy", &snap);
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
            // The guard is held in a local rather than returned as a trailing
            // expression: a trailing temporary lives until the end of the block,
            // which is after `st` is dropped, and the borrow checker rejects that.
            let prev = {
                let st = app.state::<AppState>();
                let held = st.workbuddy.lock().unwrap().clone();
                held
            };
            let snap = read_once(&prev, &mut tokens);
            if snap.status == "error" || snap.status == "stale" {
                crate::applog(&format!("workbuddy: {}", snap.note));
            }
            broadcast(&app, snap);
            sleep_interruptible(POLL_SECS);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn session_json(access: &str, uid: &str) -> serde_json::Value {
        serde_json::json!({
            "auth": { "accessToken": access, "expiresAt": NOW + 3_600_000, "domain": "tencent.com" },
            "account": { "uid": uid, "enterpriseId": "", "departmentFullName": "", "accountType": "personal" }
        })
    }

    #[test]
    fn a_readable_session_carries_its_account() {
        let (session, reason) = inspect_session(&session_json("tok", "u-1"), NOW);
        let s = session.expect("a session");
        assert_eq!(s.user_id, "u-1");
        assert_eq!(s.account_type, "personal");
        assert!(reason.is_none());
        assert!(!s.expired(NOW));
    }

    /// The whole reason the read reasons exist: telling the user to sign in cannot fix a sealed
    /// credential, so it must not be reported as a missing sign-in.
    #[test]
    fn a_sealed_credential_is_not_a_missing_sign_in() {
        let value = serde_json::json!({
            "auth": { "accessToken": { "$wbEncrypted": 1, "envelope": "AAAA" } },
            "account": { "uid": "u-1" }
        });
        let (session, reason) = inspect_session(&value, NOW);
        assert!(session.is_none());
        assert_eq!(reason, Some(ReadReason::Encrypted));
    }

    /// A sealed refresh token must not condemn an otherwise usable session.
    #[test]
    fn a_sealed_refresh_token_is_tolerated() {
        let value = serde_json::json!({
            "auth": {
                "accessToken": "tok",
                "refreshToken": { "$wbEncrypted": 1, "envelope": "AAAA" }
            },
            "account": { "uid": "u-1" }
        });
        let (session, reason) = inspect_session(&value, NOW);
        assert!(session.is_some());
        assert!(reason.is_none());
    }

    #[test]
    fn an_incomplete_or_unreadable_session_says_which() {
        let missing = serde_json::json!({ "auth": { "accessToken": "" }, "account": { "uid": "u" } });
        assert_eq!(inspect_session(&missing, NOW).1, Some(ReadReason::Incomplete));
        let not_an_object = serde_json::json!([1, 2, 3]);
        assert_eq!(inspect_session(&not_an_object, NOW).1, Some(ReadReason::Malformed));
    }

    #[test]
    fn an_expiring_session_is_expired_a_little_early() {
        let value = serde_json::json!({
            "auth": { "accessToken": "tok", "expiresAt": NOW + 10_000 },
            "account": { "uid": "u" }
        });
        let (session, _) = inspect_session(&value, NOW);
        assert!(session.unwrap().expired(NOW), "inside the skew it is already over");
    }

    fn personal_body(status: i64, size: f64, remain: f64, used: Option<f64>) -> serde_json::Value {
        let mut account = serde_json::Map::new();
        account.insert("Status".into(), serde_json::json!(status));
        account.insert("AccountId".into(), serde_json::json!("acct-1"));
        account.insert("CycleCapacitySizePrecise".into(), serde_json::json!(size));
        account.insert("CycleCapacityRemainPrecise".into(), serde_json::json!(remain));
        if let Some(u) = used {
            account.insert("CycleCapacityUsedPrecise".into(), serde_json::json!(u));
        }
        serde_json::json!({
            "code": 0,
            "data": { "Response": { "Data": { "Accounts": [serde_json::Value::Object(account)] } } }
        })
    }

    #[test]
    fn an_active_package_becomes_the_credits_window() {
        let reading = parse_personal_usage(&personal_body(0, 200000.0, 173000.0, Some(27000.0))).unwrap();
        match reading {
            Reading::Metered { used, limit, remaining, .. } => {
                assert_eq!(limit, 200000.0);
                assert_eq!(remaining, 173000.0);
                assert!((used - 27000.0).abs() < 1e-9);
            }
            other => panic!("expected a metered reading, got {other:?}"),
        }
        let reading =
            Reading::Metered { used: 27000.0, limit: 200000.0, remaining: 173000.0, resets_at: None };
        let w = window_of(&reading).expect("a window");
        assert_eq!(w.id, "credits");
        assert_eq!(w.label, "Credits");
        assert!((w.used - 0.135).abs() < 1e-9);
    }

    /// History packages are not spendable, so they are not part of the balance.
    #[test]
    fn history_packages_are_left_out_of_the_balance() {
        let body = serde_json::json!({ "data": { "Response": { "Data": { "Accounts": [
            { "Status": 3, "CycleCapacitySizePrecise": 999999.0, "CycleCapacityRemainPrecise": 999999.0 },
            { "Status": 0, "CycleCapacitySizePrecise": 100.0, "CycleCapacityRemainPrecise": 40.0 }
        ] } } } });
        match parse_personal_usage(&body).unwrap() {
            Reading::Metered { used, limit, .. } => {
                assert_eq!(limit, 100.0);
                assert!((used - 60.0).abs() < 1e-9);
            }
            other => panic!("expected a metered reading, got {other:?}"),
        }
    }

    /// A partial aggregate would look plausible while dropping a package the user can still spend.
    #[test]
    fn an_unusable_active_package_fails_the_whole_reading() {
        let body = serde_json::json!({ "data": { "Response": { "Data": { "Accounts": [
            { "Status": 0, "CycleCapacitySizePrecise": 100.0, "CycleCapacityRemainPrecise": 40.0 },
            { "Status": 0, "CycleCapacitySizePrecise": "not a number", "CycleCapacityRemainPrecise": 5.0 }
        ] } } } });
        assert!(parse_personal_usage(&body).is_err());
    }

    #[test]
    fn an_account_with_no_active_package_stays_a_configured_row() {
        let body = serde_json::json!({ "data": { "Response": { "Data": { "Accounts": [
            { "Status": 3, "CycleCapacitySizePrecise": 10.0, "CycleCapacityRemainPrecise": 1.0 }
        ] } } } });
        assert!(matches!(parse_personal_usage(&body).unwrap(), Reading::Unmetered(_)));
    }

    #[test]
    fn a_response_without_an_accounts_array_is_refused() {
        assert!(parse_personal_usage(&serde_json::json!({ "code": 0 })).is_err());
    }

    /// Used is taken from what the package states, and a package that states only a remainder
    /// still yields a spend rather than a zero.
    #[test]
    fn a_package_without_an_explicit_spend_derives_one() {
        match parse_personal_usage(&personal_body(0, 500.0, 200.0, None)).unwrap() {
            Reading::Metered { used, .. } => assert!((used - 300.0).abs() < 1e-9),
            other => panic!("expected a metered reading, got {other:?}"),
        }
    }

    #[test]
    fn a_remainder_above_the_capacity_cannot_go_over() {
        match parse_personal_usage(&personal_body(0, 100.0, 400.0, Some(0.0))).unwrap() {
            Reading::Metered { remaining, used, .. } => {
                assert_eq!(remaining, 100.0);
                assert_eq!(used, 0.0);
            }
            other => panic!("expected a metered reading, got {other:?}"),
        }
    }

    #[test]
    fn an_enterprise_plan_reports_its_cycle_reset() {
        let body = serde_json::json!({
            "data": { "limitNum": 2000, "credit": 780, "cycleResetTime": "2026-10-01T00:00:00Z" }
        });
        match parse_enterprise_usage(&body).unwrap() {
            Reading::Metered { used, limit, remaining, resets_at } => {
                assert_eq!(limit, 2000.0);
                assert_eq!(used, 780.0);
                assert_eq!(remaining, 1220.0);
                assert!(resets_at.is_some());
            }
            other => panic!("expected a metered reading, got {other:?}"),
        }
    }

    /// The vendor spells an unlimited plan as -1; that is not an error and not a 0 % ring.
    #[test]
    fn an_unlimited_enterprise_plan_is_reported_as_unmetered() {
        let body = serde_json::json!({ "data": { "limitNum": -1, "credit": 12, "cycleResetTime": null } });
        assert!(matches!(parse_enterprise_usage(&body).unwrap(), Reading::Unmetered(_)));
    }

    #[test]
    fn an_enterprise_reply_without_a_limit_is_refused() {
        assert!(parse_enterprise_usage(&serde_json::json!({ "data": { "credit": 12 } })).is_err());
        assert!(parse_enterprise_usage(&serde_json::json!({ "data": { "limitNum": 100 } })).is_err());
        assert!(parse_enterprise_usage(&serde_json::json!({ "data": { "limitNum": -7, "credit": 1 } })).is_err());
    }

    #[test]
    fn the_package_window_is_written_the_way_the_request_asks_for_it() {
        assert_eq!(format_local(0).len(), 19, "YYYY-MM-DD HH:MM:SS");
        assert!(format_local(0).contains(' '));
    }

    // ---------------- a credential the user pasted in ----------------

    /// A pasted token usually arrives the way it was copied out of a network panel. It is cleaned
    /// rather than refused, because refusing sends the user back to copy the same string again with
    /// no idea which part was wrong.
    #[test]
    fn a_pasted_token_is_cleaned_of_its_scheme_and_its_spacing() {
        assert_eq!(normalized_token("  Bearer abc123  ").as_deref(), Some("abc123"));
        assert_eq!(normalized_token("abc123").as_deref(), Some("abc123"));
        assert_eq!(normalized_token("   "), None);
        assert_eq!(normalized_token("Bearer "), None);
        assert_eq!(normalized_token(""), None);
    }

    /// A pasted credential builds the same session shape the app's own file does, minus the expiry it
    /// cannot know — and "no stated expiry" must not read as "expired", or the balance would never be
    /// requested at all.
    #[test]
    fn a_pasted_credential_names_no_expiry_and_is_usable() {
        let credential = ManualCredential {
            access_token: "  Bearer tok-1 ".into(),
            user_id: "u-1".into(),
            enterprise_id: String::new(),
            account_type: String::new(),
        };
        let session = session_from_credential(&credential);
        assert_eq!(session.token, "tok-1");
        assert_eq!(session.user_id, "u-1");
        assert_eq!(session.account_type, "personal");
        assert_eq!(session.expires_at, 0);
        assert!(!session.expired(NOW), "a token with no stated expiry is not expired");
    }

    /// The enterprise id is the field that selects the endpoint, so it has to survive the paste.
    #[test]
    fn an_enterprise_credential_keeps_its_id() {
        let credential = ManualCredential {
            access_token: "tok".into(),
            enterprise_id: " ent-9 ".into(),
            user_id: String::new(),
            account_type: "enterprise".into(),
        };
        let session = session_from_credential(&credential);
        assert_eq!(session.enterprise_id, "ent-9");
        assert_eq!(session.account_type, "enterprise");
    }

    /// The percentage is not the answer to "how much is left". The absolute remainder has to reach
    /// the window, and be named, or the card cannot print it.
    #[test]
    fn the_credits_window_carries_the_absolute_remainder() {
        let reading =
            Reading::Metered { used: 27_000.0, limit: 200_000.0, remaining: 173_000.0, resets_at: None };
        let window = window_of(&reading).expect("a metered reading draws a window");
        assert_eq!(window.id, "credits");
        assert!((window.used - 0.135).abs() < 1e-9, "the proportion is still the proportion");
        assert_eq!(window.remaining, Some(173_000.0));
        assert_eq!(window.unit.as_deref(), Some("credits"));
    }

    /// An unmetered plan has no remainder to report, and so no window to report it on.
    #[test]
    fn an_unmetered_plan_draws_no_window() {
        assert!(window_of(&Reading::Unmetered("unlimited".into())).is_none());
    }
}
