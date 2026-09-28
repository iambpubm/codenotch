//! DeepSeek's account balance, and the key it is read with.
//!
//! This is the one vendor figure the app can state exactly. WorkBuddy's Credits are exact too, but
//! sealed behind a key its own runtime holds; a harness's token total is this machine's arithmetic
//! times a published rate card, which is an estimate by construction. DeepSeek answers
//! `GET /user/balance` to anybody holding an API key, and answers with the real remainder.
//!
//! The key is the easy part, which is the exact mirror of WorkBuddy. Nothing seals it — the harness
//! keeps it in the clear in `~/.dsh/.credentials.yaml` under `refs.DEEPSEEK_API_KEY` — and somebody
//! who does not run the harness can mint one on the platform in under a minute. So this module reads
//! a key where it finds one and takes a pasted one when it cannot, in that order; either way the
//! credential goes nowhere but `api.deepseek.com`.
//!
//! # Why there is a top-up figure as well as a balance
//!
//! A remainder on its own has no percentage in it. The endpoint publishes what is left and says
//! nothing about what was put in, so a ring drawn from the balance alone would be a fraction of
//! nothing. The one person who knows the denominator is the account's owner, so it is a setting:
//! supply it and the ring means "used this much of what you have paid in", leave it blank and the
//! card prints the remainder as a figure and draws no ring. It is deliberately not inferred from
//! transaction history, because there is none to read and a guessed denominator would put a
//! confident-looking arc under a number nobody could check.

use std::path::PathBuf;
use std::time::Duration;

use crate::usage::LimitWindow;

/// The public API host. Not the platform's web host: `/user/balance` lives here, and a key is the
/// only thing it asks for. Unlike WorkBuddy's gateway this one does not inspect the `User-Agent`, but
/// one is still sent — a client that names itself is answered by every vendor the same way.
const ENDPOINT: &str = "https://api.deepseek.com";
const BALANCE_PATH: &str = "/user/balance";
const FETCH_TIMEOUT_SECS: u64 = 10;
const USER_AGENT: &str = concat!("Codenotch/", env!("CARGO_PKG_VERSION"));

/// Where a key came from. Kept because "why is the balance missing" and "where is it reading that
/// from" are different questions, and only this answers the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Pasted into Settings and stored in this app's own profile.
    Pasted,
    /// The harness's own credentials file, which stores it in the clear.
    Harness,
    /// Exported in the environment, for a machine set up that way.
    Environment,
}

/// Its own file rather than a field in `config.json`, for the same reason WorkBuddy's credential has
/// one: that document is rewritten wholesale by the settings window and handed straight back to the
/// page, and a key does not belong anywhere near that path.
fn credential_path() -> PathBuf {
    crate::config::config_path().with_file_name("deepseek-credential.json")
}

/// What the person supplied. The key is what the request needs; the top-up figure is only ever used
/// to draw a ring, and is optional because the balance reads fine without it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ManualCredential {
    #[serde(default)]
    pub api_key: String,
    /// Everything ever paid into the account, in the account's own currency. The endpoint publishes
    /// the remainder and no denominator, so this is the only way a fraction exists at all.
    #[serde(default)]
    pub topup_total: Option<f64>,
}

/// The balance the vendor reports, in the account's currency.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub currency: String,
    pub total: f64,
    pub granted: f64,
    pub topped_up: f64,
    pub available: bool,
}

// ---------------- the key ----------------

/// `DEEPSEEK_API_KEY` out of the harness's credentials file.
///
/// The file is YAML and this is not a YAML parser — it is a scan for one key on one line, which is
/// all that is needed and all that should be trusted here. Pulling in a YAML dependency to read a
/// single scalar would add a parser, its grammar and its edge cases to a read path that has none of
/// its own. The shape it looks for is the one the harness writes:
///
/// ```text
/// refs:
///   DEEPSEEK_API_KEY: sk-…
/// ```
///
/// A commented-out mention is skipped rather than matched — the word appears in this project's own
/// documentation, and a file that says `# DEEPSEEK_API_KEY: sk-your-key-here` must not be read as a
/// credential. Quotes are stripped, since a value that is quoted in the file is still the same key.
fn key_from_credentials_yaml(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.trim() != "DEEPSEEK_API_KEY" {
            continue;
        }
        // A trailing comment is not part of the value; a key never contains a `#`.
        let value = value.split('#').next().unwrap_or("").trim();
        let value = value.trim_matches(|c| c == '"' || c == '\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// The harness's credentials file, when there is one.
fn harness_key() -> Option<String> {
    let file = crate::dsh::home()?.join(".credentials.yaml");
    let text = std::fs::read_to_string(file).ok()?;
    key_from_credentials_yaml(&text)
}

/// The key to read the balance with, and where it came from.
///
/// A pasted key outranks everything: it is the one source the person chose deliberately, and if it
/// is being refused the answer is to say so rather than to silently fall back to another key and
/// report a balance from an account they did not mean.
pub fn resolved_key() -> Option<(String, Source)> {
    if let Some(key) = read_credential().and_then(|c| normalized_key(&c.api_key)) {
        return Some((key, Source::Pasted));
    }
    if let Some(key) = std::env::var("DEEPSEEK_API_KEY").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        return Some((key, Source::Environment));
    }
    harness_key().map(|key| (key, Source::Harness))
}

/// Whether a balance could be read at all, without going near the network.
pub fn key_available() -> bool {
    resolved_key().is_some()
}

/// The key exactly as it should be sent. A pasted value usually arrives whole, but people paste
/// around it — with the scheme, with a stray pair of quotes from a JSON snippet, with a trailing
/// newline — and rejecting those would send them back to copy the same string again.
fn normalized_key(raw: &str) -> Option<String> {
    let mut words = raw.split_whitespace();
    let first = words.next()?;
    let key = if first.eq_ignore_ascii_case("bearer") { words.next()? } else { first };
    let key = key.trim_matches(|c| c == '"' || c == '\'');
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

/// The file as it stands, key or no key.
///
/// A file carrying only a top-up figure is a real state and not a broken one: it is what a machine
/// with the harness ends up with, where the key is read from the harness and the person has only
/// said what they paid in. Requiring a key here would make that arrangement unsaveable.
pub fn read_credential() -> Option<ManualCredential> {
    let text = std::fs::read_to_string(credential_path()).ok()?;
    let mut credential = serde_json::from_str::<ManualCredential>(&text).ok()?;
    // Normalised on the way out as well as in, so a hand-edited file cannot leave the word `Bearer`
    // standing where a key should be. A value that normalises to nothing becomes no key rather than
    // a credential named after its scheme.
    credential.api_key = normalized_key(&credential.api_key).unwrap_or_default();
    Some(credential)
}

/// Whether a key is stored *here*, as opposed to found elsewhere. This is what the settings row
/// offers a Remove button for, so it has to mean "there is a key in this file" and nothing else — a
/// row offering to remove a key that came from the harness would be offering to do nothing.
pub fn credential_saved() -> bool {
    read_credential().map(|c| !c.api_key.is_empty()).unwrap_or(false)
}

/// What the account has been paid in all told, if the person said. Read whether or not a key was
/// pasted, because this is the one figure that is never available from anywhere else.
pub fn topup_total() -> Option<f64> {
    read_credential().and_then(|c| c.topup_total).filter(|t| t.is_finite() && *t > 0.0)
}

/// What a save produces, given what was stored and what was asked for.
///
/// Apart from the write so those "leave it alone" rules can be tested without touching a file — and
/// they are worth testing, because both are the kind of rule that is only ever noticed after it has
/// silently thrown away something a person typed.
fn merged(
    existing: &ManualCredential,
    api_key: Option<&str>,
    topup_total: Option<f64>,
) -> Result<ManualCredential, String> {
    let key = match api_key {
        Some(raw) => normalized_key(raw).ok_or_else(|| "the API key is empty".to_string())?,
        None => existing.api_key.clone(),
    };
    let topup = match topup_total {
        Some(t) if t.is_finite() && t > 0.0 => Some(t),
        // A zero or negative figure is an emptied field with a number left in it, so it clears what
        // was there rather than standing as a denominator of nothing.
        Some(_) => None,
        // Not mentioned at all: keep what was stored, the same way a missing key keeps its own.
        None => existing.topup_total,
    };
    Ok(ManualCredential { api_key: key, topup_total: topup })
}

/// Stores a key, a top-up figure, or both.
///
/// `api_key` of `None` means "leave the key alone" rather than "clear it": somebody who only wants
/// to fill in the top-up should not have to paste a key they never needed, and the field being left
/// blank is how they say so. An explicit empty string is refused, since that is a mistake rather
/// than an omission. Clearing has its own function.
pub fn save_credential(api_key: Option<&str>, topup_total: Option<f64>) -> Result<(), String> {
    let existing = read_credential().unwrap_or_default();
    let credential = merged(&existing, api_key, topup_total)?;
    let text = serde_json::to_string_pretty(&credential).map_err(|e| e.to_string())?;
    // On a fresh install nothing has written to the config directory yet, so pasting a key can be
    // the very first thing this program is asked to do and the directory is not there to write into.
    let path = credential_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    crate::dsh::request_refresh();
    Ok(())
}

/// Removes the file. A missing file is success: what matters is the state afterwards, not who made it.
pub fn forget_credential() -> Result<(), String> {
    match std::fs::remove_file(credential_path()) {
        Ok(()) => {
            crate::dsh::request_refresh();
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::dsh::request_refresh();
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

// ---------------- reading the balance ----------------

/// A number the vendor sent as a string. DeepSeek quotes its balances (`"total_balance": "40.29"`)
/// rather than sending JSON numbers, which is a documented quirk of the endpoint and not a mistake
/// to route around by refusing to read it.
fn number(value: Option<&serde_json::Value>) -> f64 {
    match value {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// The vendor's own document, read into the one currency that matters.
///
/// An account can hold several currencies. The card has room for one balance, so CNY is preferred
/// when it is there — it is the account's home currency and the one the rate card is quoted in — and
/// otherwise the first entry stands. Failing to find any currency at all is an error rather than a
/// zero, because a made-up `¥0` is worse than saying the answer could not be read.
pub fn parse_balance(body: &serde_json::Value) -> Result<Balance, String> {
    let infos = body
        .get("balance_infos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "the balance response has no balance_infos array".to_string())?;
    let chosen = infos
        .iter()
        .find(|b| b.get("currency").and_then(|c| c.as_str()) == Some("CNY"))
        .or_else(|| infos.first())
        .ok_or_else(|| "the balance response lists no currency".to_string())?;
    let currency = chosen
        .get("currency")
        .and_then(|c| c.as_str())
        .unwrap_or("CNY")
        .to_string();
    Ok(Balance {
        currency,
        total: number(chosen.get("total_balance")),
        granted: number(chosen.get("granted_balance")),
        topped_up: number(chosen.get("topped_up_balance")),
        available: body.get("is_available").and_then(|v| v.as_bool()).unwrap_or(true),
    })
}

/// One reading from the vendor. Errors are strings because every caller does the same thing with
/// them: keeps the last good figure and moves on. Nothing here is worth failing a refresh over.
pub fn fetch(key: &str) -> Result<Balance, String> {
    let url = format!("{}{}", ENDPOINT, BALANCE_PATH);
    let response = ureq::get(&url)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {}", key))
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            // 401 is the endpoint saying the key is the problem, which is worth saying precisely:
            // it is the difference between "paste a fresh key" and "try again later".
            ureq::Error::Status(401, _) => "the key was refused (401)".to_string(),
            ureq::Error::Status(code, _) => format!("the balance endpoint answered {code}"),
            other => other.to_string(),
        })?;
    let body: serde_json::Value = response.into_json().map_err(|e| e.to_string())?;
    parse_balance(&body)
}

/// The balance as a window, so the card and the ring treat it like any other reading.
///
/// `unmetered` is the honest case: without a denominator there is no fraction, and a `0 %` ring over
/// a full account would be a picture of something that is not true. The window still carries the
/// remainder, which is the whole point — `leftCopy` prints it and the ring stays undrawn.
pub fn window(balance: &Balance, topup_total: Option<f64>) -> LimitWindow {
    // A top-up below the current balance is a stale figure, not a denominator: it would read as more
    // money left than was ever paid in. Treated as absent so the card shows the remainder plainly.
    let denominator = topup_total.filter(|t| t.is_finite() && *t > 0.0 && *t >= balance.total);
    let (used, unmetered) = match denominator {
        Some(total) => (((total - balance.total) / total).clamp(0.0, 1.0), false),
        None => (0.0, true),
    };
    LimitWindow {
        id: "balance".into(),
        label: "Balance".into(),
        used,
        resets_at: None,
        remaining: Some(balance.total),
        unit: Some(balance.currency.clone()),
        unmetered,
        ..Default::default()
    }
}

/// For doctor: where the key came from and what the account holds. Never the key itself.
pub fn probe() -> String {
    let Some((key, source)) = resolved_key() else {
        return "DeepSeek: no API key (paste one in Settings, or run the harness once)".into();
    };
    let from = match source {
        Source::Pasted => "pasted in Settings",
        Source::Harness => "the harness's credentials file",
        Source::Environment => "DEEPSEEK_API_KEY in the environment",
    };
    match fetch(&key) {
        Ok(b) => format!(
            "DeepSeek: key from {}, balance {} {:.2} ({} paid in, {} granted)",
            from, b.currency, b.total, b.topped_up, b.granted
        ),
        Err(e) => format!("DeepSeek: key from {}, but the balance could not be read — {}", from, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_one_the_harness_wrote_in_the_clear() {
        // The shape the harness writes, holding fixtures that are obviously fixtures. This is the
        // one place in the repository where a working credential could be pasted in by accident, and
        // a scanner that cannot tell the two apart is doing exactly what it should.
        let text = "version: 1\nrecords:\n  client-connection/browser-session:\n    kind: grant\n    payload:\n      version: 1\n      secret: the-session-secret-not-the-key\nrefs:\n  DEEPSEEK_API_KEY: sk-fixture-never-a-real-key\n";
        assert_eq!(
            key_from_credentials_yaml(text).as_deref(),
            Some("sk-fixture-never-a-real-key"),
            "the secret under records is not the key, and must not be mistaken for one"
        );
    }

    #[test]
    fn a_commented_mention_is_not_a_credential() {
        assert_eq!(key_from_credentials_yaml("# DEEPSEEK_API_KEY: sk-your-key-here\n"), None);
        assert_eq!(key_from_credentials_yaml("  #DEEPSEEK_API_KEY: sk-x\n"), None);
        assert_eq!(key_from_credentials_yaml(""), None);
        assert_eq!(key_from_credentials_yaml("refs: {}\n"), None);
    }

    #[test]
    fn a_quoted_value_loses_its_quotes_and_its_comment() {
        assert_eq!(key_from_credentials_yaml("refs:\n  DEEPSEEK_API_KEY: \"sk-abc\"\n").as_deref(), Some("sk-abc"));
        assert_eq!(key_from_credentials_yaml("refs:\n  DEEPSEEK_API_KEY: 'sk-abc'\n").as_deref(), Some("sk-abc"));
        assert_eq!(key_from_credentials_yaml("refs:\n  DEEPSEEK_API_KEY: sk-abc # mine\n").as_deref(), Some("sk-abc"));
    }

    #[test]
    fn a_pasted_key_is_cleaned_of_its_scheme_and_its_spacing() {
        assert_eq!(normalized_key("  Bearer sk-abc  ").as_deref(), Some("sk-abc"));
        assert_eq!(normalized_key("sk-abc").as_deref(), Some("sk-abc"));
        assert_eq!(normalized_key("\"sk-abc\"").as_deref(), Some("sk-abc"));
        // Only a scheme carries no key, so it must not be stored as one — the same trap WorkBuddy's
        // credential fell into.
        assert_eq!(normalized_key("Bearer"), None);
        assert_eq!(normalized_key("Bearer "), None);
        assert_eq!(normalized_key("   "), None);
        assert_eq!(normalized_key(""), None);
    }

    fn stored(key: &str, topup: Option<f64>) -> ManualCredential {
        ManualCredential { api_key: key.into(), topup_total: topup }
    }

    #[test]
    fn a_save_that_does_not_mention_the_key_leaves_the_stored_one_alone() {
        // The harness supplies the key on this machine, so the person fills in the top-up and never
        // touches the key field. Reading that omission as a removal would break the reading.
        let out = merged(&stored("sk-abc", None), None, Some(100.0)).expect("a valid save");
        assert_eq!(out.api_key, "sk-abc");
        assert_eq!(out.topup_total, Some(100.0));
    }

    #[test]
    fn a_save_that_does_not_mention_the_top_up_leaves_the_stored_one_alone() {
        let out = merged(&stored("sk-abc", Some(100.0)), Some("sk-new"), None).expect("a valid save");
        assert_eq!(out.api_key, "sk-new");
        assert_eq!(out.topup_total, Some(100.0), "pasting a fresh key must not forget the top-up");
    }

    #[test]
    fn an_explicitly_empty_key_is_refused_rather_than_read_as_no_change() {
        // A blank field is an omission and means "keep"; a field somebody typed a space into and
        // saved is a mistake, and the two must not take the same path.
        assert!(merged(&stored("sk-abc", None), Some(""), None).is_err());
        assert!(merged(&stored("sk-abc", None), Some("Bearer"), None).is_err());
        assert!(merged(&stored("sk-abc", None), Some("   "), None).is_err());
    }

    #[test]
    fn a_top_up_of_zero_clears_rather_than_dividing_by_nothing() {
        let out = merged(&stored("sk-abc", Some(100.0)), None, Some(0.0)).expect("a valid save");
        assert_eq!(out.topup_total, None);
        let negative = merged(&stored("sk-abc", Some(100.0)), None, Some(-1.0)).expect("a valid save");
        assert_eq!(negative.topup_total, None);
    }

    fn body(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("a real vendor document")
    }

    #[test]
    fn the_balance_is_read_from_the_vendors_own_document() {
        // Exactly the shape the endpoint answers with, strings and all.
        let doc = body(
            r#"{"is_available":true,"balance_infos":[
                 {"currency":"CNY","total_balance":"40.29","granted_balance":"0.00","topped_up_balance":"40.29"}]}"#,
        );
        let b = parse_balance(&doc).expect("a readable balance");
        assert_eq!(b.currency, "CNY");
        assert_eq!(b.total, 40.29);
        assert_eq!(b.granted, 0.00);
        assert_eq!(b.topped_up, 40.29);
        assert!(b.available);
    }

    #[test]
    fn cny_is_preferred_when_the_account_holds_more_than_one() {
        let doc = body(
            r#"{"is_available":true,"balance_infos":[
                 {"currency":"USD","total_balance":"1.50"},
                 {"currency":"CNY","total_balance":"40.29"}]}"#,
        );
        assert_eq!(parse_balance(&doc).expect("readable").total, 40.29);
        // With no CNY entry the first one stands, rather than the reading failing.
        let only_usd = body(r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"1.50"}]}"#);
        let b = parse_balance(&only_usd).expect("readable");
        assert_eq!(b.currency, "USD");
        assert_eq!(b.total, 1.5);
    }

    #[test]
    fn a_document_with_no_currency_is_an_error_rather_than_a_zero() {
        assert!(parse_balance(&body(r#"{"is_available":true}"#)).is_err());
        assert!(parse_balance(&body(r#"{"is_available":true,"balance_infos":[]}"#)).is_err());
    }

    #[test]
    fn an_account_the_vendor_calls_unavailable_still_reads() {
        let doc = body(
            r#"{"is_available":false,"balance_infos":[{"currency":"CNY","total_balance":"0.00"}]}"#,
        );
        let b = parse_balance(&doc).expect("a readable balance");
        assert!(!b.available, "the flag travels so the card can say what the vendor said");
        assert_eq!(b.total, 0.0);
    }

    fn reading(total: f64) -> Balance {
        Balance { currency: "CNY".into(), total, granted: 0.0, topped_up: total, available: true }
    }

    #[test]
    fn without_a_top_up_the_ring_stays_undrawn_and_the_remainder_is_still_there() {
        let w = window(&reading(40.29), None);
        assert!(w.unmetered, "no denominator, so no percentage to draw");
        assert_eq!(w.used, 0.0);
        assert_eq!(w.remaining, Some(40.29), "the figure is the point even without the ring");
        assert_eq!(w.unit.as_deref(), Some("CNY"));
        assert_eq!(w.id, "balance");
    }

    #[test]
    fn a_top_up_gives_the_fraction_something_to_be_a_fraction_of() {
        let w = window(&reading(40.29), Some(100.0));
        assert!(!w.unmetered);
        assert!((w.used - 0.5971).abs() < 1e-4, "{}", w.used);
        assert_eq!(w.remaining, Some(40.29));
    }

    #[test]
    fn a_top_up_that_cannot_be_a_denominator_draws_no_ring() {
        // Below the balance: a stale figure, and reading it as a fraction would claim more is left
        // than was ever paid in.
        assert!(window(&reading(40.29), Some(10.0)).unmetered);
        assert!(window(&reading(40.29), Some(0.0)).unmetered, "zero is an empty field, not a denominator");
        assert!(window(&reading(40.29), Some(-5.0)).unmetered);
        assert!(window(&reading(40.29), Some(f64::NAN)).unmetered);
        // Exactly the balance is a valid denominator: nothing spent yet.
        let w = window(&reading(40.29), Some(40.29));
        assert!(!w.unmetered);
        assert_eq!(w.used, 0.0);
    }

    #[test]
    fn the_currency_the_vendor_named_is_the_unit_the_card_prints() {
        let mut b = reading(12.5);
        b.currency = "USD".into();
        assert_eq!(window(&b, None).unit.as_deref(), Some("USD"));
    }
}
