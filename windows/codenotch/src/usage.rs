//! The reading every provider publishes — and nothing else.
//!
//! `LimitWindow` and `UsageSnapshot` are the whole contract between a provider module
//! (`workbuddy.rs`, `codex.rs`, `cursor.rs`, `dsh.rs`, `glm.rs`, `opencode.rs`,
//! `antigravity.rs`, `agy_cli.rs`) and the three surfaces that draw it: the notch, the tray
//! menu and the settings window. A provider owns its wire format and its credential; this file
//! owns only the shape they agree on.
//!
//! Status vocabulary, which the pages switch on by name:
//!
//! | status | meaning |
//! | --- | --- |
//! | `ok` | a fresh reading |
//! | `stale` | an older reading kept on purpose — never invent a number on failure |
//! | `needsAuth` | there is nothing usable to read: signed out, or no credential found |
//! | `error` | the provider or the network refused, and there is no older reading to keep |
//! | `absent` | the client is not installed at all, so no cell is drawn for it |
//! | `none` | installed and readable, but nothing is metered to show |
//!
//! The store each provider persists its snapshot in lives with that provider
//! (`%APPDATA%\codenotch\<provider>.json`), so nothing here touches the disk.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LimitWindow {
    pub id: String,
    pub label: String,
    /// 0.0–1.0 (fraction used)
    pub used: f64,
    /// Reset time, ms epoch (None = unknown)
    pub resets_at: Option<u64>,
    /// A pure count window: the client publishes a number but no denominator (Antigravity's
    /// requests today, DeepSeek Harness's token spend). The cell prints the number and the ring
    /// draws only its track, because a percentage of an unpublished limit is a guess.
    #[serde(default)]
    pub count: Option<i64>,
    /// What `count` is a number *of*, for a provider whose unit is not requests — `tokens` for
    /// DeepSeek Harness. None keeps the original wording, so an Antigravity reading written by an
    /// older build still reads the way it always did.
    #[serde(default)]
    pub unit: Option<String>,
    /// The number is ours, not the vendor's (upstream fidelity=.derived) — the card adds a ~ prefix
    #[serde(default)]
    pub derived: bool,
    /// The heading the window sits under on the card, for a provider that reports the same windows
    /// for several things (Antigravity: a 5-hour and a weekly lane per model family). None = ungrouped
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageSnapshot {
    /// ok | stale | needsAuth | error | absent | none
    pub status: String,
    pub windows: Vec<LimitWindow>,
    pub fetched_at: u64,
    pub note: String,
    /// Kept from the shape the Claude adapter used: a provider that is being rate limited says
    /// until when, and the tray waits it out instead of spending another request. Providers that
    /// do not back off leave it at zero, and an older persisted snapshot still deserialises.
    #[serde(default)]
    pub backoff_until: u64,
}
