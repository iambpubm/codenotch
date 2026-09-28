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
    /// What this window's numbers are *of*, where the unit is not requests: `tokens` for DeepSeek
    /// Harness's counts, `credits` for WorkBuddy's remainder. None keeps the original wording, so an
    /// Antigravity reading written by an older build still reads the way it always did.
    #[serde(default)]
    pub unit: Option<String>,
    /// The number is ours, not the vendor's (upstream fidelity=.derived) — the card adds a ~ prefix
    #[serde(default)]
    pub derived: bool,
    /// The heading the window sits under on the card, for a provider that reports the same windows
    /// for several things (Antigravity: a 5-hour and a weekly lane per model family). None = ungrouped
    #[serde(default)]
    pub group: Option<String>,
    /// What is left, as an absolute number rather than a fraction — WorkBuddy's Credits are the case
    /// this exists for. The ring can only ever draw a proportion, and a proportion does not answer
    /// "how much do I have left"; a provider that publishes the absolute figure fills this in, and a
    /// provider that does not leaves it None rather than inventing one. `unit` names the thing.
    #[serde(default)]
    pub remaining: Option<f64>,
    /// What this window's usage came to in money, when the provider's spend can be worked out from
    /// what it already wrote (DeepSeek Harness: its own transcripts times the vendor's rate card).
    /// On the window rather than beside the snapshot so the figure travels with the span it belongs
    /// to — a card that showed one number for three different spans would be lying about two of them.
    #[serde(default)]
    pub cost: Option<f64>,
}

/// One kind of token, and what it cost. Kept apart rather than summed because the vendor prices them
/// apart: DeepSeek charges fifty times more for a cache-miss input token than a cache-hit one, and
/// four times more again for an output token, so a single blended rate would be wrong in every
/// direction at once.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostPart {
    /// What this slice is, already worded for the card ("Cache-hit input").
    pub label: String,
    pub tokens: i64,
    pub cost: f64,
}

/// What the local history says the spend was, in money.
///
/// This is an estimate, not a bill, and the distinction is not cosmetic: DeepSeek publishes no usage
/// or billing endpoint — only `/user/balance` and `/models` — so the only arithmetic available is
/// the tokens this machine recorded times the vendor's published rate card. A number no vendor
/// confirmed is marked `estimated` and the card says so, rather than being dressed up as a bill.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostEstimate {
    /// ISO code the rate card is published in: "CNY" or "USD".
    pub currency: String,
    /// The breakdown the windows are built from, over the widest span shown.
    pub parts: Vec<CostPart>,
    /// The rate card this used, in the vendor's own terms, so the figure can be checked by hand.
    pub rate_note: String,
    /// True when no vendor confirmed the number — the only value this build writes.
    pub estimated: bool,
    /// The span `parts` covers, worded as the window above it is ("Last 30 days").
    pub window: String,
    /// Models that carried tokens but have no rate in this build's table. Their usage is left out of
    /// every figure rather than guessed at, and naming them is how the card says so.
    #[serde(default)]
    pub unpriced: Vec<String>,
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
    /// What the local history says this cost, when the provider is one whose spend can be worked out
    /// from what it already wrote. None everywhere else, so nothing claims a figure it cannot know.
    #[serde(default)]
    pub cost: Option<CostEstimate>,
}
