//! The WorkBuddy desktop app's live credential, read out of its own process memory.
//!
//! WorkBuddy 5.6.0 and later seal `auth.accessToken` and `auth.refreshToken` where they are kept
//! (`{"$wbEncrypted":1,"envelope":…}` — an envelope whose key lives inside a native module that is
//! compiled into the app). The file therefore says *which* sign-in session the app is on but not
//! what its tokens are. That is a deliberate design rather than a defect, and reading the file is a
//! dead end — which is why the settings row used to ask for a token to be pasted by hand.
//!
//! The token is not sealed in memory. The app has to put it in an `Authorization` header, so it is
//! sitting in its address space as plain text while it runs. This module reads it back out.
//!
//! Two things make that a lookup rather than a guess:
//!
//! * **The session id is the anchor.** `auth.sessionState` is *not* sealed, so "which credential is
//!   the app using right now" has a written-down answer. Only tokens whose `sid` claim equals that
//!   id are kept. A process holds several sessions' worth at once, and picking the wrong one behaves
//!   exactly like a working credential the server keeps refusing — the afternoon that costs is the
//!   reason this check is not skipped.
//! * **The algorithm names the role.** `RS*`/`ES*`/`PS*` is asymmetric, which is what an access
//!   token signed by an identity provider looks like; `HS*` is symmetric, which is the refresh
//!   handle. Length or position would both be guesswork.
//!
//! Everything here is read-only, talks to no network, and is pointed at `WorkBuddy.exe` and nothing
//! else. The one place that decides whether to call it is `workbuddy::read_once`.

use std::time::{Duration, Instant};

/// How long a failed import is left alone before it is worth trying again. The scan is not free —
/// it reads a few hundred megabytes — and a session that cannot be found once will not be found
/// again a minute later. Success needs no timer: the credential is on disk by then.
const RETRY_AFTER: Duration = Duration::from_secs(600);

/// The executable whose memory is read. Matched against the file name only, case-insensitively.
const PROCESS_NAME: &str = "WorkBuddy.exe";

/// Which of the two tokens a JWT is, decided by its signing algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Signed by the identity provider; this is what the billing endpoint accepts.
    Access,
    /// The symmetric handle spent at the renewal route.
    Refresh,
}

// ---------------------------------------------------------------- pure logic

/// Base64url without padding, as a JWT segment is written.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        // Padding is not written in a JWT, but a value copied out of somewhere else may carry it.
        if byte == b'=' {
            break;
        }
        accumulator = (accumulator << 6) | value(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// The three non-empty, dot-separated segments of a JWT. `None` for anything else, which is what
/// keeps a random `eyJ…`-looking run of bytes in a heap out of the results.
fn jwt_parts(token: &str) -> Option<(&str, &str)> {
    let mut segments = token.split('.');
    let header = segments.next()?;
    let payload = segments.next()?;
    let signature = segments.next()?;
    if segments.next().is_some() || header.is_empty() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    Some((header, payload))
}

fn decoded_claim(token: &str, segment: usize) -> Option<serde_json::Value> {
    let parts = jwt_parts(token)?;
    let raw = if segment == 0 { parts.0 } else { parts.1 };
    let bytes = base64url_decode(raw)?;
    serde_json::from_slice::<serde_json::Value>(&bytes).ok()
}

/// The sign-in session a token belongs to. `sid` is what the identity provider writes; the
/// snake_case spelling is accepted so a different provider version does not silently read as
/// "no match".
pub fn session_id(token: &str) -> Option<String> {
    let claims = decoded_claim(token, 1)?;
    let id = claims.get("sid").or_else(|| claims.get("session_state"))?;
    id.as_str().map(str::to_string)
}

pub fn expires_at(token: &str) -> u64 {
    decoded_claim(token, 1)
        .and_then(|claims| claims.get("exp").and_then(|exp| exp.as_u64()))
        .unwrap_or(0)
}

fn algorithm(token: &str) -> Option<String> {
    decoded_claim(token, 0)?
        .get("alg")
        .and_then(|alg| alg.as_str())
        .map(str::to_string)
}

pub fn role_of(token: &str) -> Option<Role> {
    let algorithm = algorithm(token)?.to_ascii_uppercase();
    if algorithm.starts_with("RS") || algorithm.starts_with("ES") || algorithm.starts_with("PS") {
        Some(Role::Access)
    } else if algorithm.starts_with("HS") {
        Some(Role::Refresh)
    } else {
        None
    }
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.'
}

/// Shortest run worth trying to read as a JWT. `eyJ` on its own turns up all over ordinary base64,
/// and a real credential is never this small.
const MIN_TOKEN_LEN: usize = 40;

/// Every JWT-shaped run in a block of raw memory.
///
/// Anchored on `eyJ`, which is what base64url of `{"` always starts with, and bounded by the token
/// alphabet, so a credential stored next to other text comes back whole and nothing else does.
///
/// A run is read as one token or discarded — never cut into pieces. Two credentials sitting back to
/// back with nothing between them read as a single five-segment run, and the boundary between them
/// is genuinely not in the bytes: a signature's length is not written down anywhere. `eyJ` cannot
/// stand in for it either, because a payload is JSON too and starts the same way. Guessing would
/// mean handing back a credential with a truncated signature, which fails at the server and looks
/// like a bad paste. Missing one costs nothing: a live credential exists in several copies inside a
/// process at once — one scan of one process has turned up five.
pub fn tokens_in(data: &[u8]) -> Vec<&str> {
    let mut found = Vec::new();
    let mut index = 0usize;
    while index + 3 <= data.len() {
        if data[index] == b'e' && data[index + 1] == b'y' && data[index + 2] == b'J' {
            let mut end = index;
            while end < data.len() && is_token_byte(data[end]) {
                end += 1;
            }
            if end - index >= MIN_TOKEN_LEN {
                if let Ok(text) = std::str::from_utf8(&data[index..end]) {
                    if jwt_parts(text).is_some() {
                        found.push(text);
                    }
                }
            }
            // Past the whole run, so a rejected candidate is not re-examined byte by byte.
            index = end.max(index + 1);
        } else {
            index += 1;
        }
    }
    found
}

/// Whether `candidate` is a later issue than `current` — used to prefer the newest of several
/// tokens sharing one session id. An empty `current` counts as "nothing yet".
///
/// The length tie-break is not cosmetic. The scan runs from `eyJ` to the end of the token run,
/// and *every byte of a real JWT belongs to the token alphabet*, so a run can only ever be a
/// **superset** of a real token — it can never cut one short. Measured on a live process, the
/// byte sitting right after a refresh token happened to be a token byte, so the run came back
/// one character too long: the signature segment went from 86 characters to 87, i.e. 64 bytes to
/// 65, while HS512's HMAC is always 64 — a token the server must reject. Both copies carry the
/// same `exp`, so comparing `exp` alone left the winner up to whichever copy the scan reached
/// first. Preferring the shorter of two equally-fresh candidates resolves it deterministically.
/// Two genuinely different tokens issued in the same second are both valid, so a wrong pick there
/// costs nothing, whereas picking an over-long copy always fails.
fn newer(candidate: &str, current: &str) -> bool {
    if current.is_empty() {
        return true;
    }
    let (candidate_exp, current_exp) = (expires_at(candidate), expires_at(current));
    if candidate_exp != current_exp {
        return candidate_exp > current_exp;
    }
    candidate.len() < current.len()
}

// ---------------------------------------------------------------- the scan

/// Files the attempt clock, so a session that cannot be found is not hunted for on every poll.
static LAST_ATTEMPT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn due() -> bool {
    let mut last = LAST_ATTEMPT.lock().unwrap();
    let now = Instant::now();
    match *last {
        Some(at) if now.duration_since(at) < RETRY_AFTER => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

/// The credential the running app is using, or `None`.
///
/// `session_state` is the id read from the app's own (unsealed) `auth.sessionState`. It is required:
/// without it there is nothing to match against, and returning "some token" would be the wrong
/// answer with extra steps.
pub fn read_session_credentials(session_state: &str) -> Option<(String, String)> {
    let wanted = session_state.trim();
    if wanted.is_empty() {
        return None;
    }
    for pid in platform::pids_named(PROCESS_NAME) {
        if let Some(pair) = platform::scan(pid, wanted) {
            if !pair.0.is_empty() || !pair.1.is_empty() {
                return Some(pair);
            }
        }
    }
    None
}

/// `read_session_credentials`, rate limited and reporting the id it used.
///
/// Returns `(access, refresh)` plus the session id the pair belongs to, so the caller can write the
/// session alongside the tokens and tell later whether a stored credential is still current.
pub fn import(session_state: Option<&str>) -> Option<(String, String, String)> {
    let wanted = session_state?.trim().to_string();
    if wanted.is_empty() || !due() {
        return None;
    }
    let (access, refresh) = read_session_credentials(&wanted)?;
    Some((access, refresh, wanted))
}

// ---------------------------------------------------------------- windows

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;

    /// `HANDLE`. A failed call answers with null, which `is_null` reads.
    type Handle = *mut c_void;

    const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_VM_READ: u32 = 0x0010;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_PRIVATE: u32 = 0x20000;
    const PAGE_GUARD: u32 = 0x100;

    /// One read at a time. Large enough that the per-call overhead disappears, small enough to keep
    /// a copy of the process's few gigabytes from being held all at once.
    const CHUNK: usize = 1 << 20;
    /// Regions above this are not worth copying: they are reserved arenas, not heaps.
    const MAX_REGION: usize = 64 << 20;
    /// A ceiling on how much of one process is read, so a pathological layout cannot turn a poll
    /// into a minutes-long stall.
    const BUDGET_PER_PROCESS: u64 = 2 << 30;
    /// x64 user-space limit.
    const MAX_ADDRESS: usize = 0x0000_7fff_ffff_ffff;
    const MAX_PROCESSES: usize = 8192;

    /// `MEMORY_BASIC_INFORMATION` on x64: two pointers, then a DWORD, a WORD plus its padding, a
    /// SIZE_T, then three DWORDs and the tail padding. Laid out explicitly rather than left to the
    /// compiler, and asserted at compile time — a wrong size here would read the wrong fields and
    /// could only ever show up as "no credential found".
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition_id: u16,
        _padding0: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        memory_type: u32,
        _padding1: u32,
    }

    const _: () = assert!(size_of::<MemoryBasicInformation>() == 48);

    // kernel32 directly rather than through the `windows` crate: the crate's own binding for
    // `VirtualQueryEx` has changed shape between releases, and this machine has no Rust toolchain to
    // check a signature against. Declaring the entry points here keeps the ABI something this file
    // states outright instead of something a dependency update can move.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32EnumProcesses(process_ids: *mut u32, bytes: u32, needed: *mut u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn QueryFullProcessImageNameW(
            handle: Handle,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn VirtualQueryEx(
            handle: Handle,
            address: *const c_void,
            information: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
        fn ReadProcessMemory(
            handle: Handle,
            base: *const c_void,
            buffer: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> i32;
    }

    /// Pages worth copying: readable and not guarded. The low byte is the base protection; the bits
    /// above it are modifiers (`PAGE_NOCACHE` and friends) that do not change readability.
    fn readable(protect: u32) -> bool {
        matches!(protect & 0xff, 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80)
    }

    fn executable_name(process_id: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
            if handle.is_null() {
                return None;
            }
            let mut name = [0u16; 260];
            let mut length = name.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, name.as_mut_ptr(), &mut length);
            CloseHandle(handle);
            if ok == 0 || length == 0 {
                return None;
            }
            let full = String::from_utf16_lossy(&name[..(length as usize).min(name.len())]);
            full.rsplit(|c| c == '\\' || c == '/').next().map(str::to_string)
        }
    }

    /// Pids whose executable's file name matches, case-insensitively.
    pub fn pids_named(wanted: &str) -> Vec<u32> {
        let mut buffer = vec![0u32; MAX_PROCESSES];
        let mut needed = 0u32;
        let ok = unsafe {
            K32EnumProcesses(
                buffer.as_mut_ptr(),
                (buffer.len() * size_of::<u32>()) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Vec::new();
        }
        let count = (needed as usize / size_of::<u32>()).min(buffer.len());
        buffer[..count]
            .iter()
            .copied()
            .filter(|&process_id| process_id != 0)
            .filter(|&process_id| {
                executable_name(process_id)
                    .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
            })
            .collect()
    }

    /// Walks the process's committed, readable, private pages looking for JWTs belonging to
    /// `session_state`, and returns the newest access and refresh token it saw.
    pub fn scan(process_id: u32, session_state: &str) -> Option<(String, String)> {
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                0,
                process_id,
            );
            if handle.is_null() {
                return None;
            }

            let mut access = String::new();
            let mut refresh = String::new();
            let mut information: MemoryBasicInformation = std::mem::zeroed();
            let mut buffer = vec![0u8; CHUNK];
            let mut address = 0usize;
            let mut scanned = 0u64;

            while address < MAX_ADDRESS && scanned < BUDGET_PER_PROCESS {
                let asked = VirtualQueryEx(
                    handle,
                    address as *const c_void,
                    &mut information,
                    size_of::<MemoryBasicInformation>(),
                );
                if asked == 0 {
                    break;
                }
                let size = information.region_size;
                // A zero-sized region would leave the address where it is; stopping beats spinning.
                if size == 0 {
                    break;
                }
                if information.state == MEM_COMMIT
                    && information.memory_type == MEM_PRIVATE
                    && size <= MAX_REGION
                    && information.protect & PAGE_GUARD == 0
                    && readable(information.protect)
                {
                    let mut offset = 0usize;
                    while offset < size && scanned < BUDGET_PER_PROCESS {
                        let wanted = CHUNK.min(size - offset);
                        let mut read = 0usize;
                        let ok = ReadProcessMemory(
                            handle,
                            (address + offset) as *const c_void,
                            buffer.as_mut_ptr() as *mut c_void,
                            wanted,
                            &mut read,
                        );
                        if ok != 0 && read > 0 {
                            scanned += read as u64;
                            for token in super::tokens_in(&buffer[..read]) {
                                if super::session_id(token).as_deref() != Some(session_state) {
                                    continue;
                                }
                                // A nested `if` rather than a match guard: the guard would hold a
                                // borrow of the accumulator across the assignment to it.
                                match super::role_of(token) {
                                    Some(Role::Access) => {
                                        if super::newer(token, &access) {
                                            access = token.to_string();
                                        }
                                    }
                                    Some(Role::Refresh) => {
                                        if super::newer(token, &refresh) {
                                            refresh = token.to_string();
                                        }
                                    }
                                    None => {}
                                }
                            }
                        }
                        offset += wanted;
                    }
                }
                address = address.saturating_add(size);
            }

            CloseHandle(handle);
            if access.is_empty() && refresh.is_empty() {
                None
            } else {
                Some((access, refresh))
            }
        }
    }

    /// Exposed for the layout test below.
    #[cfg(test)]
    pub fn memory_basic_information_size() -> usize {
        size_of::<MemoryBasicInformation>()
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn pids_named(_wanted: &str) -> Vec<u32> {
        Vec::new()
    }
    pub fn scan(_process_id: u32, _session_state: &str) -> Option<(String, String)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed strings, computed once: a base64url of the exact JSON each case is about. Real tokens
    // are never used in tests, and never should be.
    const ACCESS_TOKEN: &str =
        "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzaWQiOiJzZXNzLTEiLCJleHAiOjk5OTk5OTk5OTl9.c2ln";
    const REFRESH_TOKEN: &str =
        "eyJhbGciOiJIUzUxMiIsInR5cCI6IkpXVCJ9.eyJzaWQiOiJzZXNzLTEiLCJleHAiOjk5OTk5OTk5OTl9.c2ln";
    const OTHER_SESSION: &str =
        "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1MSIsInNpZCI6Im90aGVyLXNlc3MifQ.c2ln";

    /// The struct layout is asserted at compile time; this states the same number where a test
    /// runner will report it, because a size mismatch otherwise only ever shows up as silence.
    #[test]
    fn the_memory_region_struct_is_48_bytes() {
        #[cfg(windows)]
        assert_eq!(platform::memory_basic_information_size(), 48);
    }

    #[test]
    fn base64url_round_trips_a_known_segment() {
        assert_eq!(
            base64url_decode("eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9").unwrap(),
            br#"{"alg":"RS256","typ":"JWT"}"#
        );
        // The url-safe alphabet, not the standard one.
        assert_eq!(base64url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert!(base64url_decode("not base64").is_none());
    }

    #[test]
    fn a_token_is_three_non_empty_segments() {
        assert!(jwt_parts(ACCESS_TOKEN).is_some());
        assert!(jwt_parts("eyJ.x").is_none());
        assert!(jwt_parts("eyJ..x").is_none());
        assert!(jwt_parts("eyJ.a.b.c").is_none());
        assert!(jwt_parts("plain text").is_none());
    }

    #[test]
    fn the_algorithm_decides_which_token_it_is() {
        assert_eq!(role_of(ACCESS_TOKEN), Some(Role::Access));
        assert_eq!(role_of(REFRESH_TOKEN), Some(Role::Refresh));
        // Signed by neither kind of key, so it is neither kind of token.
        assert_eq!(role_of("eyJhbGciOiJub25lIn0.eyJzaWQiOiJ4In0.c2ln"), None);
        assert_eq!(role_of("garbage"), None);
    }

    #[test]
    fn the_session_claim_is_what_matches_a_credential_to_the_app() {
        assert_eq!(session_id(ACCESS_TOKEN).as_deref(), Some("sess-1"));
        assert_eq!(session_id(OTHER_SESSION).as_deref(), Some("other-sess"));
        assert_eq!(session_id("not.a.token"), None);
    }

    #[test]
    fn expiry_comes_back_as_a_number_and_zero_when_absent() {
        assert_eq!(expires_at(ACCESS_TOKEN), 9_999_999_999);
        assert_eq!(expires_at(OTHER_SESSION), 0);
    }

    #[test]
    fn a_newer_token_wins_but_an_empty_slot_always_loses_to_a_candidate() {
        assert!(newer(ACCESS_TOKEN, ""));
        assert!(!newer(ACCESS_TOKEN, ACCESS_TOKEN));
        // Same `sid`, a bigger `exp` — the later expiry still decides.
        let later =
            "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzaWQiOiJzZXNzLTEiLCJleHAiOjEwMDAwMDAwMDAwfQ.c2ln";
        assert_eq!(expires_at(later), 10_000_000_000);
        assert!(newer(later, ACCESS_TOKEN));
        assert!(!newer(ACCESS_TOKEN, later));
    }

    #[test]
    fn of_two_equally_fresh_copies_the_longer_one_is_the_one_that_ran_on_too_far() {
        // Measured on a live process: the byte right after a refresh token happened to belong to
        // the token alphabet, so the run came back one character long — signature 86 chars → 87,
        // i.e. 64 bytes → 65, and HS512's HMAC is always 64. Both copies carry the same `exp`, so
        // without this tie-break the winner was whichever copy the scan reached first.
        let over = format!("{REFRESH_TOKEN}U");
        assert_eq!(expires_at(&over), expires_at(REFRESH_TOKEN));
        assert!(newer(REFRESH_TOKEN, &over));
        assert!(!newer(&over, REFRESH_TOKEN));
    }

    #[test]
    fn the_scan_cannot_see_where_a_run_should_have_stopped_so_the_comparator_has_to() {
        // An over-long run is returned as-is — the extra character is inside the alphabet, so the
        // byte stream holds no record of the real boundary. Dropping the run instead would be wrong
        // too: the same token sits in several places, and one scan of one process cannot tell which
        // copy it is looking at. The comparator is the layer that can, and it does.
        let over = format!("{ACCESS_TOKEN}U");
        assert_eq!(tokens_in(over.as_bytes()), vec![over.as_str()]);
        assert_eq!(role_of(&over), Some(Role::Access));
        assert!(newer(ACCESS_TOKEN, &over));
    }

    #[test]
    fn scanning_finds_tokens_in_noise_and_leaves_everything_else_alone() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"\x00\x01\x02heap noise ");
        blob.extend_from_slice(ACCESS_TOKEN.as_bytes());
        blob.extend_from_slice(b" \x00\xff more noise ");
        blob.extend_from_slice(REFRESH_TOKEN.as_bytes());
        blob.extend_from_slice(b"\x00\x00");
        let found = tokens_in(&blob);
        assert_eq!(found, vec![ACCESS_TOKEN, REFRESH_TOKEN]);

        // The anchor alone is not a token: `eyJ` occurs all over ordinary base64 payloads.
        assert!(tokens_in(b"prefix eyJhbGciOiJSUzI1NiJ9 suffix").is_empty());
        assert!(tokens_in(b"eyJhbGciOiJSUzI1NiJ9.c2ln short").is_empty());
        assert!(tokens_in(b"").is_empty());
    }

    #[test]
    fn two_credentials_packed_together_are_dropped_rather_than_guessed_at() {
        // Nothing between them, so the run is five segments and there is no honest place to cut it.
        // A truncated signature would come back as a credential the server refuses, which is worse
        // than not coming back at all — the caller still has the app's other copies to read.
        let mut blob = Vec::new();
        blob.extend_from_slice(ACCESS_TOKEN.as_bytes());
        blob.extend_from_slice(ACCESS_TOKEN.as_bytes());
        assert!(tokens_in(&blob).is_empty());
    }

    #[test]
    fn a_payload_is_json_too_so_it_starts_the_same_way_and_must_not_cut_the_token() {
        // The trap this guards: `eyJ` is the prefix of *both* segments that hold JSON, so treating
        // it as a boundary silently reduces every token to its header. Real tokens look like this.
        assert!(ACCESS_TOKEN[ACCESS_TOKEN.find('.').unwrap() + 1..].starts_with("eyJ"));
        assert_eq!(tokens_in(ACCESS_TOKEN.as_bytes()), vec![ACCESS_TOKEN]);
        assert_eq!(tokens_in(REFRESH_TOKEN.as_bytes()), vec![REFRESH_TOKEN]);
    }

    #[test]
    fn an_empty_session_id_is_never_hunted_for() {
        assert!(read_session_credentials("").is_none());
        assert!(read_session_credentials("   ").is_none());
        assert!(import(None).is_none());
        assert!(import(Some("")).is_none());
    }
}
