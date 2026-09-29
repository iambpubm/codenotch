//! CyberBrain's business readings: the workbench's todos, overdue items and delivery warnings.
//!
//! Unlike every other probe in this app, the data is not on this machine. The workbench is a PWA:
//! the real records are end-to-end encrypted in the cloud, and the only readable thing is an
//! **aggregate snapshot** the workbench page itself computes and pushes on every sync
//! (`pushStats` in the web app; the `sync` cloud function stores it at `sync/{uid}/stats.json`).
//! So this module cannot compute anything locally and cannot decrypt anything — it can only go and
//! read that one file.
//!
//! Three consequences, all deliberate:
//!
//! 1. **The workbench decides how fresh this is.** The snapshot only moves while the page is
//!    open. A reading can be hours old, so every reading carries `at` — when the page pushed it —
//!    and the card says "as of ..." rather than implying it just happened.
//! 2. **It needs the network.** On failure the last good reading is kept and marked `stale`, never
//!    reported as zero: "no todos" and "we could not find out" are different statements, and only
//!    one of them would be true.
//! 3. **The lists are capped.** The snapshot has a 400 KB ceiling and the page trims each list to
//!    ten entries. A trimmed list says so (see `Bucket::cut`) and the card prints "and N more",
//!    instead of letting ten look like all of them.
//!
//! `uid` is read from the user's config and is **not** compiled in. The `getStats` op is
//! unauthenticated: whoever knows the uid can read that snapshot, details included. That is the
//! workbench's existing design, not something this module introduces — but it is the reason the
//! uid stays out of this repository, and the reason Settings says where the number goes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

/// The workbench's cloud gateway (same URL as the web app's `GATEWAY_URL` and the desktop shell's).
const GATEWAY: &str = "https://sonnet-ai-d9glodhldc474eb0c-1318171231.ap-shanghai.app.tcloudbase.com/api";

/// How often the snapshot is re-read. The page only rewrites it while it is open, so polling faster
/// than a person could plausibly edit anything buys nothing; a minute keeps the pill responsive
/// after an edit without spending a request every few seconds on a machine left running all day.
const POLL: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT_SECS: u64 = 20;
const USER_AGENT: &str = "Codenotch/1.18 (Windows)";

/// One row on a hover card: what it is, when it is due, and how far off that is.
///
/// `days` is signed and always "how many days until `date`", so a negative value means it is
/// already past. The workbench computes it (calendar days for todos and follow-ups, working days
/// for delivery warnings) and this module passes it through rather than recomputing a second
/// opinion that could disagree with the number printed beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Item {
    pub title: String,
    pub date: String,
    /// "todo" | "trip" | "schedule" | "memo" | "project" | "order"
    pub kind: String,
    /// The short word the card shows beside the title ("Project follow-up", "Overdue"...)
    pub tag: String,
    #[serde(default)]
    pub days: Option<i64>,
}

/// A count and the rows behind it. `items` is trimmed, `cut` says whether it was.
///
/// The two travel together on purpose: a card that showed a count of 12 over a list of 10 without
/// saying why would be wrong about one of them, and there is no way to tell from the count alone
/// whether the list is complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Bucket {
    pub count: i64,
    pub items: Vec<Item>,
    pub cut: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Brain {
    /// ok | stale | none | error
    pub status: String,
    pub due: Bucket,
    pub over: Bucket,
    pub soon: Bucket,
    /// Percentage of projects finished, as the workbench's own dashboard shows it.
    pub progress: i64,
    /// The workbench's version when it pushed — worth showing, because a snapshot written before
    /// this feature existed has no details and the empty list is explained by the version.
    pub ver: String,
    /// When the workbench pushed the snapshot, ms epoch. 0 = it did not say.
    pub at: u64,
    /// When this app last read it successfully, ms epoch. Kept across a failed read, so an old
    /// reading still reports its own age instead of looking fresh.
    pub read_at: u64,
    /// Why there is nothing to show, when there is nothing. Empty on a good reading.
    pub note: String,
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("cyberbrain.json")
}

fn str_of(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn int_of(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

/// An ISO-8601 stamp from the workbench, as epoch milliseconds. 0 when it cannot be read, because
/// "unknown age" is honest and a made-up timestamp is not.
fn parse_ms(iso: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .and_then(|d| u64::try_from(d.timestamp_millis()).ok())
        .unwrap_or(0)
}

fn item_of(v: &serde_json::Value) -> Item {
    Item {
        title: str_of(v, "title"),
        date: str_of(v, "date"),
        kind: str_of(v, "kind"),
        tag: str_of(v, "tag"),
        days: v.get("days").and_then(|d| d.as_i64()),
    }
}

/// Read the gateway's reply. `None` means "the workbench has not published a usable snapshot",
/// which is a different situation from a network failure and is reported differently.
///
/// A snapshot written by a workbench older than this feature has no `detail`: the counts are kept
/// and the lists come back empty rather than the whole reading being thrown away.
pub fn parse(payload: &serde_json::Value, now: u64) -> Option<Brain> {
    let data = payload.get("data")?;
    let stats = data.get("stats")?;
    if !stats.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    let detail = stats.get("detail");
    let cut = detail.and_then(|d| d.get("cut"));
    let bucket = |key: &str, count: i64| Bucket {
        count,
        items: detail
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(item_of).collect())
            .unwrap_or_default(),
        cut: cut
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };
    Some(Brain {
        status: "ok".into(),
        due: bucket("due", int_of(stats, "todos")),
        over: bucket("over", int_of(stats, "overdue")),
        soon: bucket("soon", int_of(stats, "dueSoon")),
        progress: int_of(stats, "progress"),
        ver: str_of(stats, "ver"),
        at: data.get("ts").and_then(|v| v.as_str()).map(parse_ms).unwrap_or(0),
        read_at: now,
        note: String::new(),
    })
}

/// One reading from the gateway. Errors are strings because every caller does the same thing with
/// them: keep the last good figure and move on.
pub fn fetch(uid: &str) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "op": "getStats", "tbl": "config", "uid": uid }).to_string();
    // The `sync` function reads the body as text, not as JSON, so the content type says text.
    let response = ureq::post(GATEWAY)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .set("Content-Type", "text/plain;charset=UTF-8")
        .set("Accept", "application/json")
        .set("User-Agent", USER_AGENT)
        .send_string(&body)
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("the workbench gateway answered {code}"),
            other => other.to_string(),
        })?;
    response.into_json().map_err(|e| e.to_string())
}

/// The last reading that actually succeeded, marked `stale`. Returns None when there has never
/// been one, so a caller can tell "we have an old figure" from "we have nothing at all".
pub fn load_persisted() -> Option<Brain> {
    let text = std::fs::read_to_string(store_path()).ok()?;
    let mut brain: Brain = serde_json::from_str(&text).ok()?;
    if brain.status != "ok" {
        return None;
    }
    brain.status = "stale".into();
    Some(brain)
}

fn persist(brain: &Brain) {
    if let Ok(text) = serde_json::to_string(brain) {
        let _ = std::fs::write(store_path(), text);
    }
}

/// The workbench is not configured: a real state, and the one every fresh install starts in.
fn not_configured(now: u64) -> Brain {
    Brain {
        status: "none".into(),
        read_at: now,
        note: "no workbench uid is configured".into(),
        ..Default::default()
    }
}

/// What the pill should show right now, before any network call: a reading recovered from disk, or
/// the "nothing configured yet" placeholder. Startup must not wait on the network.
///
/// The uid is handed in rather than looked up because the two answers are not interchangeable.
/// `load_persisted` cannot tell whose reading is on disk, so on its own it would hand three
/// switched-off rings to someone who had just removed their uid — the file from when it was set is
/// still there — and they would stay on screen until the first poll a moment later. With the uid in
/// hand the placeholder wins that race outright, and the leftover file is simply never read.
pub fn initial(uid: &str) -> Brain {
    // The disk is only touched when a uid is set: without one, whatever is on it belongs to a world
    // where the rings were on, and reading it would draw them again for a moment.
    let recovered = if uid.trim().is_empty() {
        None
    } else {
        load_persisted()
    };
    startup_reading(uid, crate::now_ms(), recovered)
}

/// The start-up rule, as a value rather than as a file lookup, so the case that matters — a uid that
/// was removed while a reading from when it was set is still on disk — can be tested directly.
fn startup_reading(uid: &str, now: u64, recovered: Option<Brain>) -> Brain {
    if uid.trim().is_empty() {
        return not_configured(now);
    }
    // An empty `note` on the placeholder is deliberate: nothing is wrong, the workbench simply has
    // not published anything yet, and that is not a reason to print.
    recovered.unwrap_or(Brain {
        status: "none".into(),
        ..Default::default()
    })
}

fn keep_last(now: u64, why: &str) -> Brain {
    match load_persisted() {
        Some(mut brain) => {
            brain.note = why.to_string();
            brain
        }
        // Nothing to fall back on, and `read_at` stays where it was: a failed read must not make
        // an empty reading look like a fresh one.
        None => Brain {
            status: "error".into(),
            read_at: now,
            note: why.to_string(),
            ..Default::default()
        },
    }
}

fn read_once(uid: &str) -> Brain {
    let now = crate::now_ms();
    if uid.is_empty() {
        return not_configured(now);
    }
    match fetch(uid) {
        Ok(payload) => match parse(&payload, now) {
            Some(brain) => {
                persist(&brain);
                brain
            }
            None => keep_last(now, "the workbench has not published a snapshot yet"),
        },
        Err(e) => keep_last(now, &e),
    }
}

/// The uid from the config, trimmed. Read fresh on every use: the whole point of the field in
/// Settings is that typing a number takes effect without restarting Codenotch.
fn uid_of(app: &AppHandle) -> String {
    let state = app.state::<AppState>();
    let cfg = state.cfg.lock().unwrap();
    cfg.brain_uid.trim().to_string()
}

/// Whether two readings say the same thing.
///
/// `read_at` is left out on purpose. It is this app's own bookkeeping and it moves on every
/// successful read, so comparing it would make each tick look like news and re-render a pill that
/// had not changed a character — the opposite of what the comparison is for.
fn same_reading(a: &Brain, b: &Brain) -> bool {
    a.status == b.status
        && a.due == b.due
        && a.over == b.over
        && a.soon == b.soon
        && a.progress == b.progress
        && a.ver == b.ver
        && a.at == b.at
        && a.note == b.note
}

/// Hand a reading to both ends of the app: the pill, through an event, and the settings window,
/// which asks for it with `get_cyberbrain`. The stored copy is always current; the event only fires
/// when the reading actually moved.
fn publish(app: &AppHandle, next: &Brain, last: &mut Option<Brain>) {
    {
        let state = app.state::<AppState>();
        *state.brain.lock().unwrap() = next.clone();
    }
    let moved = last.as_ref().map(|prev| !same_reading(prev, next)).unwrap_or(true);
    if moved {
        let _ = app.emit("cyberbrain", next);
    }
    *last = Some(next.clone());
}

/// Poll the snapshot forever. An empty uid skips the request entirely rather than asking a gateway
/// a question whose only answer could be 401 — and that is a real state, not a failure: no uid
/// means the three business rings are switched off.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Option<Brain> = None;
        loop {
            let next = read_once(&uid_of(&app));
            publish(&app, &next, &mut last);
            std::thread::sleep(POLL);
        }
    });
}

/// Read once, now, instead of waiting for the next tick.
///
/// Called from `set_brain_uid`. Without it someone who has just typed their id into Settings would
/// see nothing for up to a minute, which reads as "the number was wrong" rather than as "not yet".
/// The caller runs this on its own thread: it is a network round trip.
pub fn refresh(app: &AppHandle) {
    let next = read_once(&uid_of(app));
    // Nothing to compare against, so this always speaks: an answer is exactly what was asked for.
    let mut last: Option<Brain> = None;
    publish(app, &next, &mut last);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape the workbench actually pushes (v0.49.104+), trimmed to two rows.
    fn payload_with_detail() -> serde_json::Value {
        json!({
            "code": 0,
            "data": {
                "ts": "2026-09-29T00:25:59.820Z",
                "avatar": "data:image/png;base64,AAAA",
                "stats": {
                    "ok": true, "todos": 12, "overdue": 3, "dueSoon": 1, "progress": 8,
                    "ver": "0.49.104",
                    "detail": {
                        "due": [
                            {"title": "催越南杰信尾款", "date": "2026-09-21", "kind": "todo", "tag": "工作", "days": -8},
                            {"title": "寄样品到河内", "date": "2026-10-02", "kind": "todo", "tag": "工作", "days": 3}
                        ],
                        "over": [
                            {"title": "越南杰信 · JE-100", "date": "2026-09-17", "kind": "order", "tag": "订单交期", "days": -12}
                        ],
                        "soon": [
                            {"title": "南宁华宇 · HY-20", "date": "2026-10-01", "kind": "order", "tag": "提醒", "days": 2}
                        ],
                        "cut": {"due": true, "over": false, "soon": false}
                    }
                }
            }
        })
    }

    #[test]
    fn the_three_counts_and_their_rows_all_arrive() {
        let brain = parse(&payload_with_detail(), 1000).expect("a usable snapshot");
        assert_eq!(brain.status, "ok");
        assert_eq!(brain.due.count, 12);
        assert_eq!(brain.over.count, 3);
        assert_eq!(brain.soon.count, 1);
        assert_eq!(brain.progress, 8);
        assert_eq!(brain.ver, "0.49.104");
        assert_eq!(brain.due.items.len(), 2);
        assert_eq!(brain.over.items.len(), 1);
        assert_eq!(brain.soon.items.len(), 1);
        assert_eq!(brain.due.items[0].title, "催越南杰信尾款");
        assert_eq!(brain.due.items[0].days, Some(-8));
        assert_eq!(brain.over.items[0].tag, "订单交期");
        assert_eq!(brain.soon.items[0].days, Some(2));
    }

    /// The count and the list have to be read from separate places, so a snapshot whose list was
    /// trimmed must still hand the full count to the card — that is the number on the ring.
    #[test]
    fn a_trimmed_list_still_reports_the_count_that_was_trimmed() {
        let brain = parse(&payload_with_detail(), 1000).unwrap();
        assert_eq!(brain.due.count, 12);
        assert_eq!(brain.due.items.len(), 2);
        assert!(brain.due.cut, "12 todos over a two-row list is a trimmed list");
        assert!(!brain.over.cut);
        assert!(!brain.soon.cut);
    }

    /// The workbench pushed a real timestamp; the card needs it to say "as of ...". A stamp that
    /// cannot be parsed must read as 0 (unknown) rather than as 1970 or as now.
    #[test]
    fn the_snapshot_time_is_read_off_the_payload() {
        let brain = parse(&payload_with_detail(), 1000).unwrap();
        assert_eq!(brain.at, 1_787_003_159_820);
        assert_eq!(brain.read_at, 1000);

        let mut broken = payload_with_detail();
        broken["data"]["ts"] = json!("not a date");
        assert_eq!(parse(&broken, 1).unwrap().at, 0);

        let mut missing = payload_with_detail();
        missing["data"].as_object_mut().unwrap().remove("ts");
        assert_eq!(parse(&missing, 1).unwrap().at, 0);
    }

    /// A workbench older than this feature publishes counts and no `detail`. The counts are still
    /// worth showing, so the reading survives with empty lists — it must not be discarded.
    #[test]
    fn a_snapshot_from_before_this_feature_keeps_its_counts() {
        let payload = json!({
            "code": 0,
            "data": {
                "ts": "2026-09-19T10:00:00.000Z",
                "stats": {"ok": true, "todos": 12, "overdue": 1, "dueSoon": 0, "progress": 8, "ver": "0.49.103"}
            }
        });
        let brain = parse(&payload, 5).expect("counts without details are still a reading");
        assert_eq!(brain.due.count, 12);
        assert_eq!(brain.over.count, 1);
        assert!(brain.due.items.is_empty());
        assert!(!brain.due.cut);
        assert_eq!(brain.ver, "0.49.103");
    }

    /// `ok:false` is the page saying "I have not worked out a snapshot". Reporting zeroes from it
    /// would put a confident "0 todos" on screen for someone who has twelve.
    #[test]
    fn a_snapshot_that_was_never_computed_is_not_a_reading() {
        let payload = json!({"code": 0, "data": {"stats": {"ok": false, "err": "boom"}, "ts": "2026-09-29T00:00:00.000Z"}});
        assert!(parse(&payload, 1).is_none());
    }

    #[test]
    fn a_reply_with_no_snapshot_at_all_is_not_a_reading() {
        assert!(parse(&json!({"code": 0, "data": null}), 1).is_none());
        assert!(parse(&json!({"code": 0}), 1).is_none());
        assert!(parse(&json!({}), 1).is_none());
    }

    /// Missing or wrong-typed fields inside an otherwise good snapshot must not lose the reading:
    /// a row with no date is still a row worth showing.
    #[test]
    fn a_row_missing_its_fields_still_shows_up() {
        let payload = json!({
            "code": 0,
            "data": {"ts": "2026-09-29T00:00:00.000Z", "stats": {
                "ok": true, "todos": 1, "overdue": 0, "dueSoon": 0, "progress": 0, "ver": "0.49.104",
                "detail": {"due": [{"title": "只有标题"}], "over": [], "soon": [], "cut": {}}
            }}
        });
        let brain = parse(&payload, 1).unwrap();
        assert_eq!(brain.due.items.len(), 1);
        assert_eq!(brain.due.items[0].title, "只有标题");
        assert_eq!(brain.due.items[0].date, "");
        assert_eq!(brain.due.items[0].days, None);
        assert_eq!(brain.due.items[0].tag, "");
        assert!(!brain.due.cut, "a missing cut flag means nothing was trimmed");
    }

    /// The pill is told about real changes and about nothing else. `read_at` is this app's own
    /// clock and it moves on every successful read, so a comparison that included it would fire
    /// once a minute forever.
    #[test]
    fn only_a_real_change_counts_as_news() {
        let a = parse(&payload_with_detail(), 1000).unwrap();
        let later = parse(&payload_with_detail(), 60_000).unwrap();
        assert_ne!(a.read_at, later.read_at, "the read clock really did move");
        assert!(same_reading(&a, &later), "only the read clock moved");

        let mut fewer = a.clone();
        fewer.due.count = 11;
        assert!(!same_reading(&a, &fewer), "a count moved");

        let mut renamed = a.clone();
        renamed.due.items[0].title = "寄样品到海防".into();
        assert!(!same_reading(&a, &renamed), "a row moved");

        let mut trimmed = a.clone();
        trimmed.due.cut = false;
        assert!(!same_reading(&a, &trimmed), "a list that stopped being trimmed moved");

        let mut aging = a.clone();
        aging.status = "stale".into();
        assert!(!same_reading(&a, &aging), "a reading that went stale is news");

        let mut replaced = a.clone();
        replaced.at += 1;
        assert!(!same_reading(&a, &replaced), "a newer snapshot is news");
    }

    /// No uid is the state every fresh install is in, and it must not touch the network. It is also
    /// the reason the three rings can be absent without anything being wrong.
    #[test]
    fn no_uid_is_a_state_of_its_own_rather_than_a_failure() {
        let brain = read_once("");
        assert_eq!(brain.status, "none");
        assert_eq!(brain.due.count, 0);
        assert!(brain.due.items.is_empty());
        assert!(brain.soon.items.is_empty());
        assert!(brain.note.contains("no workbench uid"), "理由要说出来: {}", brain.note);
        assert!(brain.read_at > 0, "读数本身仍然带上自己的时刻");
    }

    /// Removing the uid has to take the rings away at the next start-up too, and the reading the
    /// last successful poll left on disk is exactly what would put them back.
    #[test]
    fn a_uid_that_was_removed_does_not_bring_back_the_reading_it_left_behind() {
        let mut kept = parse(&payload_with_detail(), 1000).unwrap();
        kept.status = "stale".into();

        let off = startup_reading("", 5000, Some(kept.clone()));
        assert_eq!(off.status, "none");
        assert_eq!(off.due.count, 0);
        assert!(off.due.items.is_empty(), "三个环一个都不该出现");
        assert!(off.soon.items.is_empty());
        assert!(off.note.contains("no workbench uid"), "理由要说出来: {}", off.note);
        assert_eq!(off.read_at, 5000);

        // A uid made only of whitespace is the same answer, not a uid.
        assert_eq!(startup_reading("   ", 5000, Some(kept.clone())).status, "none");

        // With a uid the same file is the whole point: it is used, and it says it is old.
        let on = startup_reading("2094504035110363137", 5000, Some(kept));
        assert_eq!(on.status, "stale");
        assert_eq!(on.due.count, 12);

        // A uid with nothing on disk yet is a placeholder, and a silent one: the workbench has
        // simply not published anything, which is not a failure to explain.
        let fresh = startup_reading("2094504035110363137", 5000, None);
        assert_eq!(fresh.status, "none");
        assert_eq!(fresh.due.count, 0);
        assert!(fresh.note.is_empty(), "没配和「配了但还没发」不是同一件事");
    }

    /// Counts that arrive as the wrong type are dropped to 0 rather than crashing the probe.
    #[test]
    fn counts_that_are_not_numbers_do_not_crash_the_reading() {
        let payload = json!({
            "code": 0,
            "data": {"ts": "2026-09-29T00:00:00.000Z", "stats": {"ok": true, "todos": "12", "progress": null}}
        });
        let brain = parse(&payload, 1).unwrap();
        assert_eq!(brain.due.count, 0);
        assert_eq!(brain.progress, 0);
    }
}
