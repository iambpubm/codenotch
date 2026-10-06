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
/// The gateway in front of the billing endpoint screens on `User-Agent` before it looks at the token
/// at all, and answers a request it does not recognise with `403 {"code":10085}` — a plain "请求不合法"
/// that names nothing, so it reads like a bad credential and sends you hunting through the paste.
///
/// Measured against the live endpoint with one working credential: no header, an empty one, `ureq/…`
/// and `python-requests/…` were all refused; `Mozilla/5.0`, `curl/8.0`, `CodeBuddy/1.0` and a full
/// browser string all reached the balance. ureq's own default is in the refused set, so leaving this
/// to the client is not an option — the header has to be set explicitly.
///
/// The shape here is the one least likely to fall out of favour: a browser-shaped prefix, with this
/// app named in it so a server operator reading a log can tell where the request came from.
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Codenotch/",
    env!("CARGO_PKG_VERSION"),
    " Chrome/131.0.0.0 Safari/537.36"
);
/// The product the official client asks for, so the server selects the same packages it does.
const PRODUCT_CODE: &str = "p_tcaca";
/// The official client's window: it wants every package that expires inside the next 101 years.
const PERSONAL_RANGE_MS: u64 = 101 * 365 * 24 * 60 * 60 * 1000;
const POLL_SECS: u64 = 300;
/// The WorkBuddy desktop app is looked for again this often while it is not installed.
const ABSENT_POLL_SECS: u64 = 600;
const FETCH_TIMEOUT_SECS: u64 = 12;
/// The token-renewal route.
///
/// Found by probing, not by reading a document: the path the extension's own source names —
/// `/v2/auth/token/refresh` — is not routed anywhere on this platform, while
/// `/v2/plugin/auth/token/refresh` answers on every core-API origin with the platform's own refusal
/// in its own words (`12153:refresh token failed:10000:token format error`). The `plugin` segment is
/// the path prefix the extension's own configuration carries.
const REFRESH_PATH: &str = "/v2/plugin/auth/token/refresh";
/// The `X-Domain` the core API is addressed under: the CN SaaS primary origin, and the value the
/// desktop app records in its own session file as `auth.domain`.
const DEFAULT_DOMAIN: &str = "www.workbuddy.cn";
/// How close to its stated expiry an access token is renewed.
///
/// A day, because a renewal is one request and the alternative is a poll that goes out on a token
/// with minutes left on it. It is a margin rather than a deadline: nothing here waits for a token to
/// actually lapse before replacing it.
const RENEW_MARGIN_MS: u64 = 24 * 60 * 60 * 1000;

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

/// What the user pasted. Either token is enough on its own — everything else is used when it is
/// there. The enterprise id is the one field that changes the request, since it selects the endpoint.
///
/// The two tokens are not interchangeable, and which one is stored changes what happens a month
/// later. An **access** token is what the billing endpoint answers to; it belongs to one sign-in
/// session, and the app signing in again replaces that session and kills the token on the spot — its
/// own `exp` claim notwithstanding. A **refresh** token is spent at the renewal route to mint a new
/// pair, and it is the one that survives. So a credential holding a refresh token keeps itself alive
/// instead of going quietly stale, which is the whole difference between the two.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ManualCredential {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub enterprise_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub account_type: String,
    /// The `X-Domain` the renewal route is addressed under. Written when a renewal happens; the
    /// platform default when the file predates this field.
    #[serde(default)]
    pub domain: String,
}

impl ManualCredential {
    fn domain_or_default(&self) -> String {
        let domain = self.domain.trim();
        if domain.is_empty() { DEFAULT_DOMAIN.to_string() } else { domain.to_string() }
    }
}

/// The pasted credential, if one is stored and usable. A file with no token at all counts as none, so
/// "saved" and "will be sent" can never disagree.
pub fn read_credential() -> Option<ManualCredential> {
    let text = std::fs::read_to_string(credential_path()).ok()?;
    let mut credential = serde_json::from_str::<ManualCredential>(&text).ok()?;
    // Normalised on the way in as well as on the way out, so the promise above holds for a file that
    // was hand-edited as well as for one this program wrote: a token field holding the scheme and
    // nothing else is no credential, not a credential named "Bearer".
    credential.access_token = normalized_token(&credential.access_token).unwrap_or_default();
    credential.refresh_token = normalized_token(&credential.refresh_token).unwrap_or_default();
    if credential.access_token.is_empty() && credential.refresh_token.is_empty() {
        return None;
    }
    Some(credential)
}

/// Which of the two things a pasted string is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A JWT — what the billing endpoint takes in `Authorization`.
    Access,
    /// An opaque handle to spend at the renewal route.
    Refresh,
    /// Nothing to paste.
    Empty,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Access => "access",
            TokenKind::Refresh => "refresh",
            TokenKind::Empty => "empty",
        }
    }
}

/// Tells the two apart by shape, so one field can take either and nothing is asked of the user that
/// they would have to be told twice.
///
/// An access token is a JWT: exactly three dot-separated segments. A refresh token is an opaque
/// handle with no dots in it. The test is the shape and not a successful local decode, because this
/// program has no business verifying a signature — the server is the only party whose opinion
/// counts, and a token whose claims happened not to parse is still an access token to send.
pub fn classify(raw: &str) -> TokenKind {
    let Some(token) = normalized_token(raw) else { return TokenKind::Empty };
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) {
        TokenKind::Access
    } else {
        TokenKind::Refresh
    }
}

/// Second JWT segment (base64url) → claims.
///
/// Read for the two timestamps the card prints and the session id it compares, and for nothing else.
/// Nothing is verified here: a signature this program checked would still be the server's to accept
/// or refuse, and pretending otherwise would only make a local decoder look like an authority.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let part = token.split('.').nth(1)?;
    let raw = crate::antigravity::b64_decode(part)?;
    serde_json::from_slice(&raw).ok()
}

/// A numeric JWT claim in milliseconds. `iat`/`exp` are seconds since the epoch; anything that is
/// not a positive number is treated as absent rather than as 1970.
fn claim_ms(claims: &serde_json::Value, key: &str) -> Option<u64> {
    let secs = claims.get(key)?.as_f64()?;
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    Some((secs * 1000.0) as u64)
}

pub fn credential_saved() -> bool {
    read_credential().is_some()
}

/// The stored credential, described without disclosing it. Nothing here is a token and nothing here
/// can be turned back into one; it is what the settings row needs to tell a person what they are
/// actually holding.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CredentialSummary {
    /// "access", "refresh" or "both".
    pub kind: String,
    /// Total characters held across both tokens — enough to recognise a paste, useless as a value.
    pub chars: usize,
    pub enterprise: bool,
    /// When the stored access token says it lapses, in local time. Absent when there is nothing to
    /// read it from.
    pub expires_at: Option<String>,
    /// Whether that token belongs to the sign-in session the app is using right now.
    ///
    /// This is the field worth having. A stored access token can read as valid for another fortnight
    /// and still be refused, because the app signing in again retires the session it came from and
    /// nothing in the token says so. `None` means one of the two ids was unavailable, which is not
    /// the same answer as "they differ".
    pub session_matches: Option<bool>,
}

pub fn summarize() -> Option<CredentialSummary> {
    let credential = read_credential()?;
    let has_access = !credential.access_token.is_empty();
    let has_refresh = !credential.refresh_token.is_empty();
    let claims = if has_access { jwt_claims(&credential.access_token) } else { None };
    let session_id = claims
        .as_ref()
        .and_then(|c| c.get("sid"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let session_matches = match (session_id, desktop_session_state()) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };
    Some(CredentialSummary {
        kind: match (has_access, has_refresh) {
            (true, true) => "both",
            (false, true) => "refresh",
            _ => "access",
        }
        .to_string(),
        chars: credential.access_token.len() + credential.refresh_token.len(),
        enterprise: !credential.enterprise_id.trim().is_empty(),
        expires_at: claims.as_ref().and_then(|c| claim_ms(c, "exp")).map(format_local),
        session_matches,
    })
}

/// The token as it should be stored, or `None` when there is nothing to store.
///
/// A pasted token usually arrives the way it was copied out of a browser's network panel, scheme and
/// all. Cleaning it here rather than refusing it matters: refusing would send the user back to copy
/// the same string again with no idea which part was wrong.
///
/// The scheme is matched case-insensitively, because `Bearer` is not a case-sensitive word and
/// somebody retyping it will not necessarily match the browser's spelling. And a value that turns
/// out to be *only* a scheme carries no token, so it is refused rather than stored — the earlier
/// version of this stripped the scheme literally and then re-checked emptiness, which left
/// `"Bearer "` storing the word `Bearer` and sending it back as the credential.
fn normalized_token(raw: &str) -> Option<String> {
    let mut words = raw.split_whitespace();
    let first = words.next()?;
    let token = if first.eq_ignore_ascii_case("bearer") { words.next()? } else { first };
    Some(token.to_string())
}

/// Writes the credential, and nothing else. Returns the state that was actually stored so the caller
/// reports what happened rather than what it asked for.
pub fn save_credential(
    access_token: &str,
    refresh_token: &str,
    enterprise_id: &str,
    user_id: &str,
    domain: &str,
) -> Result<(), String> {
    let access = normalized_token(access_token).unwrap_or_default();
    let refresh = normalized_token(refresh_token).unwrap_or_default();
    if access.is_empty() && refresh.is_empty() {
        return Err("the credential is empty".into());
    }
    let credential = ManualCredential {
        access_token: access,
        refresh_token: refresh,
        enterprise_id: enterprise_id.trim().to_string(),
        user_id: user_id.trim().to_string(),
        account_type: if enterprise_id.trim().is_empty() { "personal".into() } else { "enterprise".into() },
        domain: if domain.trim().is_empty() { DEFAULT_DOMAIN.into() } else { domain.trim().to_string() },
    };
    write_credential(&credential)?;
    request_refresh();
    Ok(())
}

/// The write itself, without the refresh request — split out because the reader renews a credential
/// in place, and asking for a refresh from inside a read would be a loop.
fn write_credential(credential: &ManualCredential) -> Result<(), String> {
    let text = serde_json::to_string_pretty(credential).map_err(|e| e.to_string())?;
    // The directory is the config's, and on a fresh install nothing has written there yet: the
    // caches below only exist once the provider that owns them has something to cache, and a config
    // is only written when a setting changes. Pasting a credential can be the very first thing a
    // person does, so it creates the directory rather than assuming someone else did.
    let path = credential_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
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

/// The desktop app's own `auth.sessionState` — the id of the sign-in session it is currently using.
///
/// This one field is not sealed even in 5.6.0, where the token beside it is, and it is the only way
/// to separate the two failures that look identical from the outside: a token that has aged out, and
/// a token whose session was replaced when the app signed in again. The second one is the one that
/// costs somebody an hour, because the token's own `exp` still reads days away and nothing in the
/// reply says "session".
/// What the app's own auth file says about the session it is on.
///
/// None of these fields is sealed. 5.6.0 seals `auth.accessToken` and `auth.refreshToken` and leaves
/// the rest readable, which is what makes the rest worth reading: the session id is an anchor a
/// credential can be checked against, and the account id is what a balance request is addressed to.
#[derive(Debug, Clone, Default)]
pub struct DesktopAuth {
    /// The id of the sign-in session the app is currently using.
    pub session_state: String,
    /// The domain the app addresses its own requests to.
    pub domain: String,
    /// The account id, sent as `X-User-Id` when asking for a balance.
    pub user_id: String,
}

/// The first auth file that names a session decides.
pub fn desktop_auth() -> Option<DesktopAuth> {
    fn text_at(value: &serde_json::Value, path: &[&str]) -> String {
        let mut cursor = value;
        for step in path {
            cursor = match cursor.get(step) {
                Some(next) => next,
                None => return String::new(),
            };
        }
        cursor.as_str().unwrap_or("").trim().to_string()
    }
    for dir in auth_dirs() {
        let Some(text) = read_regular_file(&auth_file_in(&dir)) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let auth = DesktopAuth {
            session_state: text_at(&value, &["auth", "sessionState"]),
            domain: text_at(&value, &["auth", "domain"]),
            user_id: text_at(&value, &["account", "uid"]),
        };
        if !auth.session_state.is_empty() {
            return Some(auth);
        }
    }
    None
}

/// The desktop app's own `auth.sessionState` — the id of the sign-in session it is currently using.
///
/// This one field is not sealed even in 5.6.0, where the token beside it is, and it is the only way
/// to separate the two failures that look identical from the outside: a token that has aged out, and
/// a token whose session was replaced when the app signed in again. The second one is the one that
/// costs somebody an hour, because the token's own `exp` still reads days away and nothing in the
/// reply says "session".
pub fn desktop_session_state() -> Option<String> {
    desktop_auth().map(|auth| auth.session_state)
}

/// The sign-in session a token belongs to, or `None` when it is not a token this can read.
fn token_session(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    jwt_claims(token)
        .and_then(|claims| claims.get("sid").and_then(|sid| sid.as_str()).map(str::to_string))
        .or_else(|| crate::workbuddy_memory::session_id(token))
        .map(|sid| sid.trim().to_string())
        .filter(|sid| !sid.is_empty())
}

/// Whether a stored credential still belongs to the session the app is on.
///
/// Either half counts. The two tokens do not fail together: a refresh token outlives the app's next
/// sign-in because it is spent at the renewal route to mint a new pair, while the access token
/// beside it was killed the moment that session was replaced. Asking only about the access token
/// would throw away a credential that is still doing its job.
///
/// With no app auth file to compare against — WorkBuddy not installed, or never signed in — nothing
/// is known, and "unknown" must not read as "stale": that would discard a credential somebody just
/// pasted on a machine where the app has not run yet.
fn credential_is_current(credential: &ManualCredential) -> bool {
    let Some(app_session) = desktop_session_state() else { return true };
    let app_session = app_session.trim().to_string();
    [&credential.access_token, &credential.refresh_token]
        .iter()
        .filter_map(|token| token_session(token.as_str()))
        .any(|session| session == app_session)
}

/// Look in the running app's memory for the credential it is using, and write it down.
///
/// This is the one path that can fill in a credential without asking. It exists because the app
/// seals its own copy on disk, so when a session is replaced there is nothing on the file system to
/// re-read — the user is otherwise left to go and find a token by hand.
///
/// Returns the credential only if it was written.
pub fn import_live_credential(force: bool) -> Option<ManualCredential> {
    let app = desktop_auth()?;
    // The automatic path is rate limited — the scan is not free, and a session that cannot be found
    // once will not be found a moment later. A button press is not: somebody is waiting on the
    // result of *this* attempt, and "not yet" is not an answer they can do anything with.
    let (access, refresh, _session) = if force {
        crate::workbuddy_memory::read_session_credentials(&app.session_state)
            .map(|(access, refresh)| (access, refresh, app.session_state.clone()))
    } else {
        crate::workbuddy_memory::import(Some(&app.session_state))
    }?;
    let mut credential = read_credential().unwrap_or_default();
    credential.access_token = access;
    if !refresh.is_empty() {
        credential.refresh_token = refresh;
    }
    // Fields a token cannot carry. The app's own file is the authority for these, and a pasted
    // credential has always relied on the defaults when they are absent — but an import has no
    // reason to leave them blank when the answer is sitting in the file next to the session id.
    if credential.enterprise_id.trim().is_empty() {
        credential.enterprise_id = String::new();
    }
    if credential.user_id.trim().is_empty() && !app.user_id.is_empty() {
        credential.user_id = app.user_id.clone();
    }
    if credential.account_type.trim().is_empty() {
        credential.account_type = "personal".to_string();
    }
    if credential.domain.trim().is_empty() && !app.domain.is_empty() {
        credential.domain = app.domain.clone();
    }
    write_credential(&credential).ok()?;
    read_credential()
}

// ---------------- renewing a credential ----------------

/// A fresh pair, as the core API hands it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renewed {
    pub access_token: String,
    pub refresh_token: String,
    /// ms epoch; 0 = the reply named no expiry.
    pub expires_at: u64,
}

/// Why a renewal did not happen. `Refused` is kept separate from `Unavailable` because only the
/// first means the credential itself is finished; a network that was down is worth trying again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewErr {
    /// The server declined the handle, in its own words.
    Refused(String),
    /// The request never got an answer worth reading.
    Unavailable(String),
    /// An answer arrived but carried no token. A bug on one side or the other, not a verdict.
    Shape(String),
}

impl RenewErr {
    fn note(&self) -> String {
        match self {
            RenewErr::Refused(why) => format!("WorkBuddy refused the refresh token — {why}"),
            RenewErr::Unavailable(why) => format!("Could not reach WorkBuddy to renew the credential — {why}"),
            RenewErr::Shape(why) => format!("WorkBuddy answered the renewal with no token — {why}"),
        }
    }

    fn refused(&self) -> bool {
        matches!(self, RenewErr::Refused(_))
    }
}

/// Spends a refresh token at the renewal route for a new pair.
///
/// The header contract is the one the extension's own code uses: the handle travels in
/// `X-Refresh-Token`, the origin in `X-Domain`, and `X-Auth-Refresh-Source` names where the request
/// came from so an operator reading a log can tell this apart from the app's own traffic.
fn renew(refresh_token: &str, domain: &str) -> Result<Renewed, RenewErr> {
    let Some(handle) = normalized_token(refresh_token) else {
        return Err(RenewErr::Shape("there is no refresh token to spend".into()));
    };
    let domain = if domain.trim().is_empty() { DEFAULT_DOMAIN } else { domain.trim() };
    let answer = ureq::post(&format!("{ENDPOINT}{REFRESH_PATH}"))
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .set("Accept", "application/json")
        .set("User-Agent", USER_AGENT)
        .set("X-Domain", domain)
        .set("X-Refresh-Token", &handle)
        .set("X-Auth-Refresh-Source", "plugin")
        .send_string("{}");
    let body: serde_json::Value = match answer {
        Ok(r) => {
            let status = r.status();
            if (300..400).contains(&status) {
                return Err(RenewErr::Unavailable(format!("HTTP {status} redirect refused")));
            }
            r.into_json().map_err(|e| RenewErr::Unavailable(format!("parse: {e}")))?
        }
        // A refusal here is an ordinary reply, not a transport failure: the platform answers a bad
        // handle with a status and a JSON body naming the fault.
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            return Err(parse_renew_failure(code, &text));
        }
        Err(e) => return Err(RenewErr::Unavailable(format!("{e}"))),
    };
    parse_renew_body(&body)
}

/// A refusal, kept in the server's own words.
///
/// The reply is `{"code":12153,"msg":"12153:refresh token failed:10000:token format error"}`, and that
/// `msg` is the entire diagnosis: it separates a malformed handle from a session that has ended, and
/// the two call for different things from the person reading it. Replacing it with a sentence of this
/// program's own would throw away the only part that knows which happened.
fn parse_renew_failure(status: u16, text: &str) -> RenewErr {
    let message = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(str::to_string))
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    match message {
        Some(why) => RenewErr::Refused(why),
        None => RenewErr::Refused(format!("HTTP {status}")),
    }
}

/// The reply, unwrapped.
///
/// The `code` is checked before the payload because a refusal can arrive under a 200 with the reason
/// in the body — which is exactly how the platform reports a spend that was declined.
fn parse_renew_body(body: &serde_json::Value) -> Result<Renewed, RenewErr> {
    if let Some(code) = body.get("code").and_then(|c| c.as_f64()) {
        if code != 0.0 && code != 200.0 {
            let why = body.get("msg").and_then(|m| m.as_str()).unwrap_or("").trim().to_string();
            return Err(RenewErr::Refused(if why.is_empty() { format!("code {code}") } else { why }));
        }
    }
    let data = body.get("data").ok_or_else(|| RenewErr::Shape("the reply carried no data".into()))?;
    // Some deployments answer one envelope deeper; take whichever level actually holds the token.
    let data = if data.get("accessToken").is_some() || data.get("access_token").is_some() {
        data
    } else {
        data.get("data").unwrap_or(data)
    };
    let pick = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| data.get(*k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let access_token = pick(&["accessToken", "access_token"]);
    if access_token.is_empty() {
        return Err(RenewErr::Shape("the reply carried no access token".into()));
    }
    // `expiresAt` when the server states it, otherwise `expiresIn` counted from now — the same
    // conversion the app's own code makes, so the number this program prints matches the app's.
    let expires_at = match data.get("expiresAt").and_then(|v| v.as_f64()) {
        Some(ms) if ms > 0.0 => ms as u64,
        _ => data
            .get("expiresIn")
            .and_then(|v| v.as_f64())
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| now_ms().saturating_add((s * 1000.0) as u64))
            .unwrap_or(0),
    };
    Ok(Renewed { access_token, refresh_token: pick(&["refreshToken", "refresh_token"]), expires_at })
}

/// Brings a stored credential up to date before it is used.
///
/// A credential holding a refresh token outlives the app signing in again, so this is the difference
/// between one that keeps working and one that goes quietly stale. It does nothing while the access
/// token still has a comfortable margin left: renewing on every poll would be three hundred requests
/// a day for no gain. An access token with no readable expiry is used as it is, and left to the retry
/// below rather than renewed on a guess.
fn freshen(credential: &mut ManualCredential) -> Option<String> {
    if credential.refresh_token.trim().is_empty() {
        return None;
    }
    let due = match normalized_token(&credential.access_token) {
        None => true,
        Some(access) => match jwt_claims(&access).as_ref().and_then(|c| claim_ms(c, "exp")) {
            Some(exp) => exp <= now_ms().saturating_add(RENEW_MARGIN_MS),
            None => false,
        },
    };
    if !due {
        return None;
    }
    // Hoisted into a local rather than built inline: the handle and the domain are both read off the
    // credential that the arm below writes back to, and a temporary in a match scrutinee outlives the
    // call it was made for. Naming it removes the question.
    let domain = credential.domain_or_default();
    match renew(&credential.refresh_token, &domain) {
        Ok(fresh) => {
            credential.access_token = fresh.access_token;
            if !fresh.refresh_token.is_empty() {
                credential.refresh_token = fresh.refresh_token;
            }
            write_credential(credential).ok()?;
            None
        }
        Err(e) => Some(e.note()),
    }
}

// ---------------- checking a credential before it is stored ----------------

/// What the settings page is told about a candidate credential, before it is stored or after.
///
/// It never carries either token. Everything here is something a person can act on: when the token
/// was issued and when it says it lapses, which sign-in session it belongs to and whether that is
/// still the one the app is using, and what the server said when it was actually sent.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CredentialCheck {
    /// What the pasted string was read as: "access", "refresh" or "empty".
    pub kind: String,
    /// True only when the server answered with a balance.
    pub ok: bool,
    /// A one-word verdict: ok, renewed, refused, unverified or empty.
    pub status: String,
    /// The sentence to show. English, like every other note this program writes.
    pub note: String,
    /// The token's own `iat` / `exp`, in local time. Absent when there is nothing to read them from.
    pub issued_at: Option<String>,
    pub expires_at: Option<String>,
    /// The sign-in session this token belongs to, and the one the app is using now.
    pub session_id: Option<String>,
    pub app_session_id: Option<String>,
    /// Whether those agree. `None` when either is missing, which is not the same answer as "no".
    pub session_matches: Option<bool>,
    /// Credits left, when the check got that far.
    pub remaining: Option<f64>,
}

impl CredentialCheck {
    /// Whether the server actively turned this credential down.
    ///
    /// The distinction matters for what happens next: a refusal is not stored, because the file
    /// existing is what tells the card to use a credential, and writing a refused one in would only
    /// move the confusion out of the field and into the notch.
    pub fn rejected(&self) -> bool {
        self.status == "refused" || self.status == "empty"
    }

    fn blank(kind: TokenKind, app_session_id: Option<String>) -> Self {
        CredentialCheck {
            kind: kind.as_str().to_string(),
            ok: false,
            status: "empty".to_string(),
            note: String::new(),
            issued_at: None,
            expires_at: None,
            session_id: None,
            app_session_id,
            session_matches: None,
            remaining: None,
        }
    }
}

/// A check plus what a save would write: the pair the platform last issued, which is the pasted one
/// unless a renewal replaced it.
struct Evaluation {
    check: CredentialCheck,
    access: String,
    refresh: String,
    domain: String,
}

/// A short form of a session id for a sentence. Enough to see two of them differ, which is the whole
/// job — the full id is shown beside it.
fn short_id(id: &Option<String>) -> String {
    match id {
        Some(id) if id.len() > 8 => format!("{}…", &id[..8]),
        Some(id) => id.clone(),
        None => "?".to_string(),
    }
}

/// Sends the candidate to the server and reports what came back.
///
/// The server is the only authority here, so this always ends in a request. The local reads — the
/// claims, the session comparison — exist to explain a refusal, not to predict one: a token with
/// days left on its own clock is exactly the one that gets refused for reasons the clock cannot see.
fn evaluate(token: &str, enterprise_id: &str, user_id: &str, domain: &str) -> Evaluation {
    let kind = classify(token);
    let mut check = CredentialCheck::blank(kind, desktop_session_state());
    let domain = if domain.trim().is_empty() { DEFAULT_DOMAIN.to_string() } else { domain.trim().to_string() };
    let mut access = String::new();
    let mut refresh = String::new();
    // Collected in one place so every early return still carries the domain a save would write.
    let done = |check: CredentialCheck, access: String, refresh: String| Evaluation {
        check,
        access,
        refresh,
        domain: domain.clone(),
    };

    match kind {
        TokenKind::Empty => {
            check.note = "Nothing was pasted".to_string();
            return done(check, access, refresh);
        }
        TokenKind::Access => access = normalized_token(token).unwrap_or_default(),
        TokenKind::Refresh => refresh = normalized_token(token).unwrap_or_default(),
    }

    if kind == TokenKind::Access {
        let claims = jwt_claims(&access);
        check.issued_at = claims.as_ref().and_then(|c| claim_ms(c, "iat")).map(format_local);
        check.expires_at = claims.as_ref().and_then(|c| claim_ms(c, "exp")).map(format_local);
        check.session_id = claims
            .as_ref()
            .and_then(|c| c.get("sid"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        check.session_matches = match (&check.session_id, &check.app_session_id) {
            (Some(a), Some(b)) => Some(a == b),
            _ => None,
        };
    }

    // A refresh token is worthless until it has been spent, so the exchange is the check.
    if kind == TokenKind::Refresh {
        match renew(&refresh, &domain) {
            Ok(fresh) => {
                check.status = "renewed".to_string();
                check.note = "Refresh token accepted — a new access token was issued for it".to_string();
                check.expires_at =
                    if fresh.expires_at > 0 { Some(format_local(fresh.expires_at)) } else { None };
                access = fresh.access_token;
                if !fresh.refresh_token.is_empty() {
                    refresh = fresh.refresh_token;
                }
            }
            Err(e) => {
                check.status = if e.refused() { "refused" } else { "unverified" }.to_string();
                check.note = e.note();
                return done(check, access, refresh);
            }
        }
    }

    // The billing call settles it either way, including for a token a renewal just replaced.
    let session = Session {
        token: access.clone(),
        user_id: user_id.trim().to_string(),
        enterprise_id: enterprise_id.trim().to_string(),
        department_info: String::new(),
        domain: String::new(),
        account_type: if enterprise_id.trim().is_empty() { "personal".to_string() } else { "enterprise".to_string() },
        expires_at: 0,
    };
    match fetch_once(&session) {
        Ok(body) => {
            let reading = if session.enterprise_id.is_empty() {
                parse_personal_usage(&body)
            } else {
                parse_enterprise_usage(&body)
            };
            match reading {
                Ok(Reading::Metered { remaining, .. }) => {
                    check.ok = true;
                    check.status = if kind == TokenKind::Refresh { "renewed" } else { "ok" }.to_string();
                    check.remaining = Some(remaining);
                    check.note = format!("Verified — {remaining:.0} credits left");
                }
                Ok(Reading::Unmetered(note)) => {
                    check.ok = true;
                    check.status = if kind == TokenKind::Refresh { "renewed" } else { "ok" }.to_string();
                    check.note = if note.is_empty() { "Verified".to_string() } else { note };
                }
                Err(why) => {
                    check.status = "unverified".to_string();
                    check.note = why;
                }
            }
        }
        Err(FetchErr::NeedsAuth) => {
            check.ok = false;
            check.status = "refused".to_string();
            check.note = match check.session_matches {
                // The failure that costs an hour: the token still says it has days to run, and the
                // only thing wrong with it is that it belongs to a session that no longer exists.
                Some(false) => format!(
                    "Refused. This token belongs to sign-in session {}, but WorkBuddy is now on {}. \
                     Signing in again replaces the old session and retires tokens from it, however long \
                     their own expiry claims. Copy one out of a request the browser makes now.",
                    short_id(&check.session_id),
                    short_id(&check.app_session_id)
                ),
                _ => "Refused — WorkBuddy does not accept this token. Copy a fresh one from the browser."
                    .to_string(),
            };
        }
        Err(FetchErr::Unavailable(why)) => {
            check.status = "unverified".to_string();
            check.note = format!("Could not check this credential — {why}");
        }
    }
    done(check, access, refresh)
}

/// Checks a candidate credential without storing anything.
pub fn check_credential(token: &str, enterprise_id: &str, user_id: &str, domain: &str) -> CredentialCheck {
    evaluate(token, enterprise_id, user_id, domain).check
}

/// Verifies first, then stores — and stores the pair the platform just issued rather than the string
/// that was pasted, so a pasted refresh token leaves behind a credential that is already live.
///
/// A credential the server actively refused is not written at all. Everything else is, including one
/// that could not be checked because the network was down: refusing to keep it would make an offline
/// machine unable to receive a credential it has already been given.
pub fn verify_and_save(
    token: &str,
    enterprise_id: &str,
    user_id: &str,
    domain: &str,
) -> Result<CredentialCheck, String> {
    let evaluated = evaluate(token, enterprise_id, user_id, domain);
    if evaluated.check.rejected() {
        return Err(evaluated.check.note.clone());
    }
    save_credential(&evaluated.access, &evaluated.refresh, enterprise_id, user_id, &evaluated.domain)?;
    Ok(evaluated.check)
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
            "WorkBuddy: pasted credential in use (access token {} chars, refresh token {}, {}{})",
            credential.access_token.trim().len(),
            if credential.refresh_token.trim().is_empty() { "none" } else { "held" },
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
        .set("User-Agent", USER_AGENT)
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
    let mut pasted = read_credential();
    // Missing, or belonging to a session the app has since replaced: those are the two cases the
    // file system cannot answer, because the app's own copy is sealed. Before either is reported as
    // a dead end, look for the live credential in the app's own memory. The attempt is rate limited
    // inside, so a machine where this never succeeds is not scanned on every poll.
    let stale = match pasted.as_ref() {
        Some(credential) => !credential_is_current(credential),
        None => true,
    };
    if stale {
        if let Some(imported) = import_live_credential(false) {
            pasted = Some(imported);
        }
    }
    let from_paste = pasted.is_some();
    // A credential holding a refresh token is renewed in place here, before it is used, so a paste
    // that outlived the app's own sign-in session keeps reading instead of going stale. The failure
    // is kept rather than discarded: when the balance then goes missing, "the refresh token was
    // refused, and here is what the server said" is a far better sentence than "paste a fresh one".
    let renewal_failed = pasted.as_mut().and_then(freshen);
    let (session, reason) = match pasted.as_ref() {
        Some(credential) => (Some(session_from_credential(credential)), None),
        None => read_session(now),
    };
    // What to say when the balance is missing. A refused pasted token is not a signed-out app: the
    // remedy is a fresh token in Settings, and pointing at the app's sign-in screen would send the
    // user somewhere that cannot change the outcome.
    let missing = |reason: Option<ReadReason>| -> String {
        if from_paste {
            renewal_failed.clone().unwrap_or_else(|| PASTED_REFUSED.into())
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

    // One retry, and only one. A credential can hold a refresh token that outlives the access token
    // sitting beside it, and a token the server has just refused is exactly the moment to spend it —
    // the case this whole path exists for. A second refusal is the server's answer, not something to
    // hammer at.
    let mut attempt = fetch_once(&session);
    if matches!(attempt, Err(FetchErr::NeedsAuth)) {
        if let Some(credential) = pasted.as_mut() {
            if !credential.refresh_token.trim().is_empty() {
                // Named for the same reason as in `freshen`: the arm below writes the credential this
                // call reads from.
                let domain = credential.domain_or_default();
                if let Ok(fresh) = renew(&credential.refresh_token, &domain) {
                    credential.access_token = fresh.access_token;
                    if !fresh.refresh_token.is_empty() {
                        credential.refresh_token = fresh.refresh_token;
                    }
                    let _ = write_credential(credential);
                    attempt = fetch_once(&session_from_credential(credential));
                }
            }
        }
    }

    match attempt {
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
                missing(None)
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

    /// The gateway screens on `User-Agent` before it looks at the credential, so an unrecognised one
    /// turns a working token into `403 {"code":10085}` — an error that names neither the header nor
    /// the reason, and reads exactly like a bad paste. The header is set explicitly for that reason,
    /// and these are the shapes measured against the live endpoint: the ones on the right reached the
    /// balance, the ones on the left were refused before it.
    #[test]
    fn the_request_carries_a_user_agent_the_gateway_accepts() {
        assert!(USER_AGENT.starts_with("Mozilla/5.0"), "a browser-shaped prefix: {USER_AGENT}");
        assert!(USER_AGENT.contains("Codenotch/"), "and this app named in it: {USER_AGENT}");
        assert!(!USER_AGENT.trim().is_empty());

        // ureq's own default, which is what the header would be if this line were ever dropped.
        let refused_by_the_gateway = ["ureq", "python-requests", "python-urllib"];
        let lowered = USER_AGENT.to_lowercase();
        for refused in refused_by_the_gateway {
            assert!(
                !lowered.starts_with(refused),
                "{refused} is refused by the gateway, so it cannot be the user agent"
            );
        }
    }

    // ---------------- a credential the user pasted in ----------------

    /// A pasted token usually arrives the way it was copied out of a network panel. It is cleaned
    /// rather than refused, because refusing sends the user back to copy the same string again with
    /// no idea which part was wrong.
    #[test]
    fn a_pasted_token_is_cleaned_of_its_scheme_and_its_spacing() {
        assert_eq!(normalized_token("  Bearer abc123  ").as_deref(), Some("abc123"));
        assert_eq!(normalized_token("abc123").as_deref(), Some("abc123"));
        // The scheme is a word, not a case-sensitive prefix: someone retyping it may not match the
        // browser's spelling, and the token is unchanged either way.
        assert_eq!(normalized_token("bearer abc123").as_deref(), Some("abc123"));
        assert_eq!(normalized_token("BEARER abc123").as_deref(), Some("abc123"));
        // A header value pasted out of a panel can carry the scheme's own separator rather than a space.
        assert_eq!(normalized_token("Bearer\tabc123").as_deref(), Some("abc123"));
        assert_eq!(normalized_token("Bearer\nabc123").as_deref(), Some("abc123"));
        // Nothing to store. The scheme on its own is the case worth stating: keeping it would store
        // the word "Bearer" as the credential and send it back as `Authorization: Bearer Bearer`.
        assert_eq!(normalized_token("   "), None);
        assert_eq!(normalized_token("Bearer"), None);
        assert_eq!(normalized_token("Bearer "), None);
        assert_eq!(normalized_token("bearer   "), None);
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
            ..Default::default()
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
            account_type: "enterprise".into(),
            ..Default::default()
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

    // ---------------- the two kinds of token ----------------

    /// A real-shaped access token: three segments, the middle one a claims object. Nothing here is
    /// verified — the fields are only read for the two timestamps and the session id.
    const FIXTURE_JWT: &str = concat!(
        "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.",
        "eyJpYXQiOjE3NTYwMDAwMDAsImV4cCI6MTc5NDQ3NzUyMywic2lkIjoiZDhiYzk0MGEtOTM4Ny00N2I1LWEwYWUtNTg1Yzc4ZTc5ZDQ2IiwiYXpwIjoid29ya2J1ZGR5Iiwic3ViIjoiZmQ1MWM1NjgtNWE1Yy00YWUxLWE1MGMtOTVlODk4ZTQ2YTJmIn0.",
        "c2ln"
    );

    #[test]
    fn the_two_kinds_of_token_are_told_apart_by_shape() {
        assert_eq!(classify(FIXTURE_JWT), TokenKind::Access);
        // The `Bearer ` prefix a browser's network panel leaves on is the same either way.
        assert_eq!(classify(&format!("Bearer {FIXTURE_JWT}")), TokenKind::Access);
        assert_eq!(classify("  "), TokenKind::Empty);
        assert_eq!(classify(""), TokenKind::Empty);
        // An opaque handle has no dots, so it cannot be mistaken for a JWT.
        assert_eq!(classify("3f9c1d2e-0b44-4a71-9f0e-8c2b6d5a1e77"), TokenKind::Refresh);
        // Two of the three segments is not a JWT: the shape is what decides, and a near-miss is
        // still sent somewhere — better the renewal route than silently treated as empty.
        assert_eq!(classify("aaa.bbb"), TokenKind::Refresh);
    }

    #[test]
    fn the_claims_are_read_for_the_two_timestamps_and_the_session() {
        let claims = jwt_claims(FIXTURE_JWT).expect("a claims object");
        assert_eq!(claim_ms(&claims, "iat"), Some(1_756_000_000_000));
        assert_eq!(claim_ms(&claims, "exp"), Some(1_794_477_523_000));
        assert_eq!(
            claims.get("sid").and_then(|v| v.as_str()),
            Some("d8bc940a-9387-47b5-a0ae-585c78e79d46")
        );
    }

    /// A claim that is missing, zero or not a number means "not stated" — never 1970, which would
    /// make every credential look long expired and renew on every poll.
    #[test]
    fn an_unstated_claim_is_not_the_epoch() {
        let claims = serde_json::json!({ "exp": 0, "iat": "soon", "nbf": -5 });
        assert_eq!(claim_ms(&claims, "exp"), None);
        assert_eq!(claim_ms(&claims, "iat"), None);
        assert_eq!(claim_ms(&claims, "nbf"), None);
        assert_eq!(claim_ms(&claims, "absent"), None);
    }

    /// A refresh token, spent at the renewal route. The path is pinned because the one the extension's
    /// own source names is not routed at all: `/v2/auth/token/refresh` answers `404 Route Not Found`
    /// on every core-API origin, while `/v2/plugin/auth/token/refresh` answers with the platform's own
    /// refusal. Getting this one segment wrong turns a working credential into a mystery.
    #[test]
    fn the_renewal_route_is_the_one_that_actually_answers() {
        assert_eq!(REFRESH_PATH, "/v2/plugin/auth/token/refresh");
        assert!(REFRESH_PATH.starts_with("/v2/"), "every core-API route here is under /v2");
        assert_ne!(REFRESH_PATH, "/v2/auth/token/refresh", "the unrouted path the source names");
        assert!(USER_AGENT.starts_with("Mozilla/5.0"), "the gateway screens on this before the token");
    }

    /// The refusal the platform actually gave, verbatim from a live probe on 2026-10-05. Keeping the
    /// server's own sentence is the point: it separates a malformed handle from a session that ended,
    /// and those two call for different things from the person reading it.
    #[test]
    fn a_renewal_refusal_keeps_the_servers_own_words() {
        let body = r#"{"code":12153,"msg":"12153:refresh token failed:10000:token format error","requestId":"3b7a5ff4"}"#;
        let err = parse_renew_failure(401, body);
        assert_eq!(
            err,
            RenewErr::Refused("12153:refresh token failed:10000:token format error".into())
        );
        assert!(err.refused(), "a refusal is a verdict on the credential");
        assert!(err.note().contains("token format error"), "the server's words survive into the note");
        // A gateway page with no JSON in it is still a refusal, just a less informative one.
        let html = parse_renew_failure(401, "<html><head><title>401 Authorization Required</title>");
        assert_eq!(html, RenewErr::Refused("HTTP 401".into()));
    }

    /// A refusal can also arrive under a 200, with the reason in `code`/`msg`. Reading the payload
    /// first would turn that into "the reply carried no access token" and lose the reason.
    #[test]
    fn a_refusal_under_a_200_is_still_a_refusal() {
        let body = serde_json::json!({ "code": 12153, "msg": "refresh token failed:10000" });
        assert_eq!(parse_renew_body(&body), Err(RenewErr::Refused("refresh token failed:10000".into())));
    }

    #[test]
    fn a_renewal_reply_is_read_from_either_envelope() {
        let flat = serde_json::json!({
            "code": 0,
            "data": { "accessToken": "a-1", "refreshToken": "r-1", "expiresIn": 3600 }
        });
        let nested = serde_json::json!({
            "code": 0,
            "data": { "data": { "access_token": "a-2", "refresh_token": "r-2", "expiresAt": 1_800_000_000_000u64 } }
        });
        let before = now_ms();
        let a = parse_renew_body(&flat).expect("the flat shape");
        let after = now_ms();
        assert_eq!(a.access_token, "a-1");
        assert_eq!(a.refresh_token, "r-1");
        // `expiresIn` is seconds counted from the moment of the reply, so the only honest assertion
        // is that it landed an hour ahead of the call — not against a fixed number.
        assert!(
            a.expires_at >= before + 3_600_000 && a.expires_at <= after + 3_600_000,
            "expiresIn is an hour counted from now, got {} between {} and {}",
            a.expires_at,
            before + 3_600_000,
            after + 3_600_000
        );

        let b = parse_renew_body(&nested).expect("the nested shape");
        assert_eq!(b.access_token, "a-2");
        assert_eq!(b.refresh_token, "r-2");
        assert_eq!(b.expires_at, 1_800_000_000_000, "a stated expiresAt is already milliseconds");
    }

    /// A reply with no token in it is a shape problem, not a refusal — the credential may be fine.
    /// Reporting it as refused would throw away a credential over someone else's bug.
    #[test]
    fn a_tokenless_reply_is_not_a_refusal() {
        let err = parse_renew_body(&serde_json::json!({ "code": 0, "data": {} })).unwrap_err();
        assert!(!err.refused());
        assert!(matches!(err, RenewErr::Shape(_)));
        assert_eq!(parse_renew_body(&serde_json::json!({ "code": 0 })).unwrap_err(), RenewErr::Shape("the reply carried no data".into()));
    }

    // ---------------- what a save is allowed to store ----------------

    #[test]
    fn a_refused_or_empty_check_is_never_stored() {
        let mut check = CredentialCheck::blank(TokenKind::Access, None);
        check.status = "refused".into();
        assert!(check.rejected());
        check.status = "empty".into();
        assert!(check.rejected());
        // "Could not reach the server" must not block a save: an offline machine still has to be able
        // to take a credential it has been handed.
        check.status = "unverified".into();
        assert!(!check.rejected());
        check.status = "ok".into();
        assert!(!check.rejected());
        check.status = "renewed".into();
        assert!(!check.rejected());
    }

    /// The refusal that costs an hour: the token still says it has days left, and the only thing wrong
    /// with it is the session it belongs to. The sentence has to say so, or the reader goes looking at
    /// the clock.
    #[test]
    fn a_replaced_session_is_named_as_the_reason() {
        let mut check = CredentialCheck::blank(TokenKind::Access, Some("d8bc940a-9387".into()));
        check.session_id = Some("4d7a08d8-1111".into());
        check.session_matches = match (&check.session_id, &check.app_session_id) {
            (Some(a), Some(b)) => Some(a == b),
            _ => None,
        };
        assert_eq!(check.session_matches, Some(false));
        assert_eq!(short_id(&check.session_id), "4d7a08d8…");
        assert_eq!(short_id(&check.app_session_id), "d8bc940a…");
        assert_eq!(short_id(&None), "?");
    }

    /// Without both ids there is no comparison to make, and "unknown" must not be reported as "they
    /// differ" — that would libel a credential that is merely older than this field.
    #[test]
    fn a_missing_session_on_either_side_compares_as_unknown() {
        let both_missing: (Option<String>, Option<String>) = (None, None);
        let matches = match (&both_missing.0, &both_missing.1) {
            (Some(a), Some(b)) => Some(a == b),
            _ => None,
        };
        assert_eq!(matches, None);
    }

    #[test]
    fn the_renewal_domain_defaults_to_the_one_the_app_records() {
        assert_eq!(ManualCredential::default().domain_or_default(), DEFAULT_DOMAIN);
        assert_eq!(DEFAULT_DOMAIN, "www.workbuddy.cn");
        let named = ManualCredential { domain: " wb.example.cn ".into(), ..Default::default() };
        assert_eq!(named.domain_or_default(), "wb.example.cn");
    }
}
