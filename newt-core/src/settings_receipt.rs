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

/// Override for where receipts land. Set by tests and by anyone who wants the
/// journal somewhere other than beside `settings.toml`.
pub const RECEIPT_PATH_ENV: &str = "NEWT_SETTINGS_RECEIPTS";

/// One applied setting change.
///
/// `from` and `to` are the setting's own value vocabulary (`vi`, `relentless`,
/// `auto`) — closed token sets, never free operator text, so a receipt cannot
/// become a second transcript of what was typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingChange {
    pub schema: String,
    /// The setting's canonical name — the `/settings <name>` deep-link token.
    pub setting: String,
    /// What it was before this change.
    pub from: String,
    /// What it is now.
    pub to: String,
    /// The route the operator actually took: `/settings`, `/vi`, `/psyche
    /// tenacity`. The same change through two verbs is two different events,
    /// and which verb was used is the part a reader cannot reconstruct.
    pub via: String,
    /// Wall-clock display claim. Append order remains the ordering ground truth.
    pub ts_claim: String,
}

impl SettingChange {
    #[must_use]
    pub fn new(setting: &str, from: &str, to: &str, via: &str) -> Self {
        Self {
            schema: SETTING_CHANGE_SCHEMA_V1.to_string(),
            setting: setting.to_string(),
            from: from.to_string(),
            to: to.to_string(),
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
/// `settings.toml` (`~/.newt/`). `None` when there is no user config dir at
/// all, which is the one case with nowhere honest to write.
#[must_use]
pub fn receipt_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(RECEIPT_PATH_ENV) {
        return Some(PathBuf::from(explicit));
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
        receipt.change.to = "nano".to_string();
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
        a.to = "nano".to_string();
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
        crate::process_env::remove_var(RECEIPT_PATH_ENV);
    }
}
