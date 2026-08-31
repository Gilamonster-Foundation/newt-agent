//! **Receipts for applied settings** (#1981) — the durable half of #1965.
//!
//! #1965's finding was not "the round cap was wrong". It was that an operator
//! raised a cap from 40 to effectively unlimited and *no record of it existed
//! anywhere*: not in config, not in a receipt, not in a turn row. The reason is
//! structural — `newt-tui/src/chat.rs` says it in a comment: "Slash commands
//! never reach the receipt path". Every knob an operator turns from the prompt
//! writes a process global and vanishes.
//!
//! This is the destination that ends that. One applied setting change is one
//! [`SettingChange`], addressed by its content and appended as a JSON line.
//!
//! # Why content-addressed
//!
//! A receipt whose integrity you cannot check is a log entry. [`SettingChange`]
//! implements `ContentAddressable`, so the line carries the id its own bytes
//! compute to and [`SettingReceipt::is_intact`] re-derives it: an edited `to`
//! field stops matching. That is the workspace's content-addressable rule
//! applied where it earns its keep — this file is evidence about authority
//! changes, which is exactly the class of record that gets edited after the
//! fact. `newt_core::agentic::content_spill` is the in-repo precedent for the
//! `canonical_form` → `to_canonical_dagcbor` one-liner.
//!
//! # Pure record, fs at the edge
//!
//! Everything except [`record`] and [`SettingReceipt::append_jsonl`] is pure,
//! following `flight_recorder`'s split ("This module is PURE: it builds and
//! serializes records. The session wires the append at the dispatch
//! boundary."). That is what keeps the unit tier fully mocked: the form's
//! callers exercise minting and rendering with no filesystem at all.
//!
//! # Best effort, never load-bearing
//!
//! [`record`] returns `None` rather than failing: an observability write must
//! never undo a setting the operator asked for. `denial_journal` takes the same
//! position for the same reason.

use content_addressable::{canonical, ContentAddressable, ContentError, ContentId};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Schema tag. Bumping it re-addresses every receipt, by construction.
pub const SETTING_CHANGE_SCHEMA_V1: &str = "newt.setting-change/v1";

/// Override for where receipts land, for anyone who wants the journal
/// somewhere other than beside `settings.toml`.
///
/// Set to the **empty string** to turn the journal off entirely. That is not a
/// courtesy switch — it is what keeps the unit tier honest: a test that
/// exercised a setting change would otherwise append to the developer's real
/// `~/.newt/receipts.jsonl`, and `test_guard::GlobalSettingsGuard` sets this
/// empty for exactly that reason.
pub const RECEIPT_PATH_ENV: &str = "NEWT_SETTINGS_RECEIPTS";

/// What a setting was, or became.
///
/// Most settings are a closed token vocabulary — `vi`, `relentless`, `auto` —
/// and those are [`Self::Token`]. The round cap is not: its value is a number
/// that only means something beside the derivation that produced it, so it
/// carries [`ToolRoundLimit`] — #1982's record, **reused rather than
/// re-declared**. "320 rounds" is a number; "320, from an override, over a
/// configured 40, under relentless" is what #1965 asked for.
///
/// `#[serde(untagged)]` is load-bearing, not tidiness: a token serializes as a
/// bare string, exactly as the plain `String` field it replaced did, so every
/// receipt already written keeps parsing AND keeps its content address. The
/// two variants are disjoint on the wire (a string versus an object), so the
/// untagged read cannot go wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    /// A value from the setting's own closed vocabulary.
    Token(String),
    /// A tool-round cap AND the derivation that produced it (#1982).
    ToolRounds(crate::tenacity::ToolRoundLimit),
}

impl From<&str> for SettingValue {
    fn from(value: &str) -> Self {
        Self::Token(value.to_string())
    }
}

impl From<String> for SettingValue {
    fn from(value: String) -> Self {
        Self::Token(value)
    }
}

impl From<crate::tenacity::ToolRoundLimit> for SettingValue {
    fn from(limit: crate::tenacity::ToolRoundLimit) -> Self {
        Self::ToolRounds(limit)
    }
}

impl std::fmt::Display for SettingValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(token) => f.write_str(token),
            Self::ToolRounds(limit) => write!(
                f,
                "{} (from {}, over a configured {})",
                limit.rounds,
                limit.source.as_str(),
                limit.configured
            ),
        }
    }
}

/// One applied setting change.
///
/// `from` and `to` are the setting's own value vocabulary — never free
/// operator text, so a receipt cannot become a second transcript of what was
/// typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingChange {
    pub schema: String,
    /// The setting's canonical name — the `/settings <name>` deep-link token.
    pub setting: String,
    /// What it was before this change.
    pub from: SettingValue,
    /// What it is now.
    pub to: SettingValue,
    /// The route the operator actually took: `/settings`, `/vi`, `/psyche
    /// tenacity`. The same change through two verbs is two different events,
    /// and which verb was used is the part a reader cannot reconstruct.
    pub via: String,
    /// Wall-clock display claim. Append order remains the ordering ground truth.
    pub ts_claim: String,
}

impl SettingChange {
    #[must_use]
    pub fn new(
        setting: &str,
        from: impl Into<SettingValue>,
        to: impl Into<SettingValue>,
        via: &str,
    ) -> Self {
        Self {
            schema: SETTING_CHANGE_SCHEMA_V1.to_string(),
            setting: setting.to_string(),
            from: from.into(),
            to: to.into(),
            via: via.to_string(),
            ts_claim: chrono::Utc::now().to_rfc3339(),
        }
    }
}

impl ContentAddressable for SettingChange {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// One journal line: a change and the address its own bytes compute to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingReceipt {
    /// The [`SettingChange`]'s `ContentId`, canonical string form.
    pub id: String,
    pub change: SettingChange,
}

impl SettingReceipt {
    /// Mint the receipt for `change` by addressing it.
    ///
    /// # Errors
    ///
    /// Propagates a canonical-encoding failure from the change.
    pub fn mint(change: SettingChange) -> Result<Self, ContentError> {
        let id = change.content_id()?.to_string();
        Ok(Self { id, change })
    }

    /// Re-derive the address from the change and compare it to the claim.
    ///
    /// This is the whole point of addressing the record: a receipt whose `to`
    /// was edited after the fact no longer computes to the id it carries.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        ContentId::from_str(&self.id)
            .ok()
            .and_then(|id| self.change.verify(&id).ok())
            .unwrap_or(false)
    }

    /// The JSON line this receipt is stored as. Pure — the fs write is
    /// separate so the unit tier can check the encoding without a file.
    ///
    /// # Errors
    ///
    /// Propagates a serialization failure.
    pub fn render_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Append this receipt to an append-only journal, creating parents.
    ///
    /// # Errors
    ///
    /// Propagates any filesystem or encoding failure.
    pub fn append_jsonl(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = self.render_line().map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }
}

/// Where receipts land: `$NEWT_SETTINGS_RECEIPTS`, else `receipts.jsonl` beside
/// `settings.toml` (so it follows `$NEWT_CONFIG_DIR` like everything else in
/// `~/.newt`).
///
/// `None` — do not write — in two cases: the override is set but empty (the
/// off switch), or there is no user config dir at all, which is the one case
/// with nowhere honest to put it.
#[must_use]
pub fn receipt_path() -> Option<PathBuf> {
    match std::env::var_os(RECEIPT_PATH_ENV) {
        Some(explicit) if explicit.is_empty() => return None,
        Some(explicit) => return Some(PathBuf::from(explicit)),
        None => {}
    }
    crate::settings::settings_path().map(|p| p.with_file_name("receipts.jsonl"))
}

/// Mint and durably record one applied setting change.
///
/// Returns the receipt that was written, or `None` when there was nowhere to
/// write or the write failed — deliberately best effort, because failing to
/// record a change must never undo the change.
#[must_use]
pub fn record(change: SettingChange) -> Option<SettingReceipt> {
    let receipt = SettingReceipt::mint(change).ok()?;
    let path = receipt_path()?;
    receipt.append_jsonl(&path).ok()?;
    Some(receipt)
}

/// Parse a journal. Corrupt or partial lines are skipped so one interrupted
/// append cannot hide the rest of the evidence (`denial_journal`'s rule).
#[must_use]
pub fn read_jsonl(body: &str) -> Vec<SettingReceipt> {
    body.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change() -> SettingChange {
        SettingChange::new("edit-mode", "vi", "emacs", "/vi")
    }

    /// The receipt carries the address its own bytes compute to.
    #[test]
    fn a_minted_receipt_is_intact() {
        let receipt = SettingReceipt::mint(change()).expect("mintable");
        assert!(receipt.is_intact());
        assert!(
            receipt.id.starts_with("bafyr"),
            "not a BLAKE3 dag-cbor CIDv1: {}",
            receipt.id
        );
    }

    /// **Anti-vacuous twin.** `is_intact` would be useless if it were `true`
    /// for anything — editing the recorded value must break the address.
    #[test]
    fn an_edited_receipt_is_not_intact() {
        let mut receipt = SettingReceipt::mint(change()).expect("mintable");
        receipt.change.to = SettingValue::Token("nano".to_string());
        assert!(
            !receipt.is_intact(),
            "a rewritten `to` still verified — the address proves nothing"
        );

        let mut forged = SettingReceipt::mint(change()).expect("mintable");
        forged.id = "bafyrgqbnotarealcontentidatallxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".to_string();
        assert!(!forged.is_intact(), "an unparsable id must not verify");
    }

    /// Equal changes address equally; a different route is a different event.
    #[test]
    fn the_route_is_part_of_the_identity() {
        let mut a = change();
        let mut b = change();
        b.ts_claim.clone_from(&a.ts_claim);
        assert_eq!(a.content_id().unwrap(), b.content_id().unwrap());

        b.via = "/settings".to_string();
        assert_ne!(
            a.content_id().unwrap(),
            b.content_id().unwrap(),
            "the same value change through two verbs must not share an address"
        );

        // ...and so is the value, obviously — but assert it, because a
        // canonical form that dropped a field would pass the check above.
        a.to = SettingValue::Token("nano".to_string());
        b = change();
        b.ts_claim.clone_from(&a.ts_claim);
        assert_ne!(a.content_id().unwrap(), b.content_id().unwrap());
    }

    /// A rendered line round-trips, and survives neighbours that do not parse.
    #[test]
    fn a_journal_skips_corrupt_lines_and_keeps_the_rest() {
        let receipt = SettingReceipt::mint(change()).expect("mintable");
        let line = receipt.render_line().expect("renderable");
        let body = format!("{line}\n{{ truncated\n{line}");
        let read = read_jsonl(&body);
        assert_eq!(read.len(), 2, "the intact lines must survive");
        assert_eq!(read[0], receipt);
        assert!(read.iter().all(SettingReceipt::is_intact));
    }

    fn rounds(
        rounds: usize,
        source: crate::tenacity::ToolRoundLimitSource,
        configured: usize,
    ) -> crate::tenacity::ToolRoundLimit {
        crate::tenacity::ToolRoundLimit {
            rounds,
            source,
            configured,
            tenacity: Some(crate::Tenacity::Relentless),
        }
    }

    /// **A token still encodes as a bare string.**
    ///
    /// This is what makes `SettingValue` a widening rather than a break: every
    /// receipt written before the enum existed still parses, and — because the
    /// canonical bytes are unchanged — still computes to the same address. An
    /// externally-tagged enum would have silently re-addressed every receipt on
    /// disk.
    #[test]
    fn a_token_value_is_wire_identical_to_the_plain_string_it_replaced() {
        let receipt = SettingReceipt::mint(change()).expect("mintable");
        let line = receipt.render_line().expect("renderable");
        assert!(line.contains(r#""from":"vi""#), "{line}");
        assert!(line.contains(r#""to":"emacs""#), "{line}");
        assert!(receipt.is_intact());
    }

    /// **The derivation is bound into the address, not decoration beside it.**
    ///
    /// #1965's complaint was that "320 rounds" was recorded without what it was
    /// measured against. Each field of the derivation is varied ALONE here —
    /// same rounds, same everything else — because a pair that differs in two
    /// fields proves nothing about either: an earlier version of this test
    /// varied `rounds` and `configured` together and passed happily with
    /// `configured` dropped from the encoding entirely.
    #[test]
    fn each_field_of_the_derivation_is_bound_into_the_address() {
        use crate::tenacity::ToolRoundLimitSource::{Config, Override, Tenacity as FromTenacity};
        let base = SettingChange::new(
            "rounds",
            rounds(40, Config, 40),
            rounds(320, Override, 40),
            "/rounds",
        );
        let vary = |mutate: &dyn Fn(&mut crate::tenacity::ToolRoundLimit)| {
            let mut other = base.clone();
            if let SettingValue::ToolRounds(limit) = &mut other.to {
                mutate(limit);
            }
            other
        };

        // 320 over a configured 40 is an ESCALATION; 320 over a configured 320
        // is not. Same number, different fact.
        let baseline = vary(&|l| l.configured = 320);
        // Which input won: an override the operator typed, or a tenacity level.
        let source = vary(&|l| l.source = FromTenacity);
        // The level in play when it happened.
        let level = vary(&|l| l.tenacity = Some(crate::Tenacity::Standard));
        // …and the number itself, obviously.
        let number = vary(&|l| l.rounds = 321);

        let id = base.content_id().unwrap();
        for (name, other) in [
            ("configured", baseline),
            ("source", source),
            ("tenacity", level),
            ("rounds", number),
        ] {
            assert_ne!(
                id,
                other.content_id().unwrap(),
                "`{name}` is not in the address — a receipt that drops it \
                 records the escalation exactly as invisibly as before"
            );
        }

        // Anti-vacuous: an unmutated copy DOES agree, so the assertions above
        // are about the payload and not about `ts_claim` moving underneath.
        assert_eq!(id, base.clone().content_id().unwrap());
    }

    /// A rounds receipt survives the journal and stays verifiable.
    #[test]
    fn a_rounds_receipt_round_trips_through_the_journal() {
        use crate::tenacity::ToolRoundLimitSource::{Config, Override};
        let change = SettingChange::new(
            "rounds",
            rounds(40, Config, 40),
            rounds(320, Override, 40),
            "/max-rounds",
        );
        let receipt = SettingReceipt::mint(change).expect("mintable");
        let read = read_jsonl(&receipt.render_line().expect("renderable"));
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], receipt);
        assert!(
            read[0].is_intact(),
            "a rounds receipt must verify like any other"
        );
        // The alias the operator actually typed survives the round trip.
        assert_eq!(read[0].change.via, "/max-rounds");
        assert_eq!(
            read[0].change.to.to_string(),
            "320 (from override, over a configured 40)"
        );
    }

    /// The path is overridable, which is also how a caller keeps the journal
    /// out of `~/.newt` (the tests that drive `record` do exactly this).
    #[test]
    fn the_journal_path_is_overridable() {
        let _lock = crate::process_env::lock();
        crate::process_env::set_var(RECEIPT_PATH_ENV, "/tmp/newt-settings-receipts-probe.jsonl");
        assert_eq!(
            receipt_path(),
            Some(PathBuf::from("/tmp/newt-settings-receipts-probe.jsonl"))
        );
        crate::process_env::set_var(RECEIPT_PATH_ENV, "");
        assert_eq!(receipt_path(), None, "empty must mean off, not the default");
        crate::process_env::remove_var(RECEIPT_PATH_ENV);
    }

    /// **The off switch actually stops the write.** Without this, `record`
    /// could ignore `receipt_path` and the unit tier would quietly append to a
    /// developer's real `~/.newt/receipts.jsonl` — which is exactly what
    /// happened before the switch existed.
    #[test]
    fn recording_with_the_journal_off_writes_nothing() {
        let _lock = crate::process_env::lock();
        crate::process_env::set_var(RECEIPT_PATH_ENV, "");
        assert!(record(change()).is_none());
        crate::process_env::remove_var(RECEIPT_PATH_ENV);
    }
}
