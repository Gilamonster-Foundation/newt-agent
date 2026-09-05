//! `newt ocap` — the durable-policy management surface (#1176 / track O).
//!
//! Today it hosts one subcommand, `propose`: fold the shadow-OCAP flight
//! recorder's capture (what a `--full-access` session actually used) into a
//! **reviewable, unsigned** `approve.toml` proposal. The operator reviews it,
//! then blesses it with `newt doctor --sign-ocap` — the only path from an
//! observed caveat to a durable grant. This closes the #1176 loop:
//! **observe → catalog → promote**, the inversion of prompt → grant.
//!
//! The fs-touching entry point ([`run`]) is a thin wrapper; the decision core
//! ([`plan_proposal`]) and the operator render ([`render`]) are pure and
//! fully unit-tested (no real fs, no wall-clock).

use std::path::{Path, PathBuf};

use clap::Subcommand;

use newt_core::denial_journal::ChainBreak;
use newt_core::flight_recorder::{read_capture_jsonl, CAPTURE_PATH_ENV};
use newt_core::ocap_propose::{in_policy_pairs, propose_from_capture, Proposal};
use newt_core::ocap_store::{build_store, CapabilityClass, PolicyFile, Verdict, VERDICTS};

#[derive(Subcommand, Debug)]
pub enum OcapCmd {
    /// Review confined denials as repair evidence. Repeated targets are folded
    /// and classified as policy gaps, shell/parser implementation work, or a
    /// grant-retry failure. This never grants authority.
    Denials {
        /// Read this journal instead of `~/.newt/denial-journal.jsonl`.
        #[arg(long, value_name = "FILE")]
        journal: Option<PathBuf>,
    },
    /// Propose durable `approve.toml` candidates from the flight-recorder
    /// capture of a prior `--full-access` (or `--yolo`) session — the observed
    /// authority a leash would have gated on. Dry-run by default (prints the
    /// proposal); `--save` records the low-danger candidates UNSIGNED, for you
    /// to review and then bless with `newt doctor --sign-ocap`. High-danger
    /// targets (interpreters, broad fs roots) are never proposed — they are
    /// reported for passkey step-up or continued prompting.
    Propose {
        /// Record the low-danger candidates into `~/.newt/ocap/approve.toml`
        /// (UNSIGNED — they do nothing until you `newt doctor --sign-ocap`).
        /// Without this, `propose` only prints what it would add.
        #[arg(long, default_value_t = false)]
        save: bool,
        /// Read the capture from this file instead of the armed default
        /// (`$NEWT_FLIGHT_RECORDER`, else `~/.newt/flight-recorder/unconfined.jsonl`).
        #[arg(long, value_name = "FILE")]
        capture: Option<PathBuf>,
    },
    /// Revoke an enrolled passkey credential.
    ///
    /// Gated by the same terminal window the enrollment ceremony uses: a
    /// headless session cannot revoke, because it cannot be asked. The row is
    /// flagged and re-signed rather than deleted, so clearing the flag by hand
    /// invalidates the signature and the credential stays dead.
    RevokeCredential {
        /// Credential handle, or any unambiguous prefix of one.
        #[arg(value_name = "CRED-SHORT")]
        handle: String,
        /// Registry bundle to edit (`~/.newt/ocap/credentials.d/<subject>.toml`).
        #[arg(long, default_value = "operator")]
        subject: String,
        /// Operator root key. Default: `~/.newt/identity.pem`.
        #[arg(long, env = "NEWT_OPERATOR_KEY", value_name = "FILE")]
        operator_key_path: Option<PathBuf>,
    },
}

/// Dispatch `newt ocap <cmd>`.
pub fn run(cmd: OcapCmd, config: Option<&Path>) -> anyhow::Result<i32> {
    match cmd {
        OcapCmd::Denials { journal } => run_denials(journal, config),
        OcapCmd::Propose { save, capture } => run_propose(save, capture, config),
        OcapCmd::RevokeCredential {
            handle,
            subject,
            operator_key_path,
        } => run_revoke_credential(&handle, &subject, operator_key_path, config),
    }
}

/// `newt ocap revoke-credential <cred-short>` — confirm at the terminal, then
/// flip and re-sign the row.
fn run_revoke_credential(
    handle: &str,
    subject: &str,
    operator_key_path: Option<PathBuf>,
    config: Option<&Path>,
) -> anyhow::Result<i32> {
    let config_path = config
        .map(Path::to_path_buf)
        .or_else(newt_core::Config::user_config_path)
        .ok_or_else(|| anyhow::anyhow!("cannot locate the newt config directory"))?;
    let key_path = match operator_key_path {
        Some(path) => path,
        None => newt_identity::default_key_path()?,
    };
    let root_key = newt_identity::load_user_key(&key_path)?;

    // The capability, not a flag: a session with no terminal cannot obtain one,
    // so it reaches the default-deny arm below instead of revoking unattended.
    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    if !newt_core::interaction_terminal::confirmed_on_terminal(
        &window,
        &newt_core::interaction_form::confirm(
            format!("revoke credential `{handle}` for `{subject}`?"),
            "",
            "yes, revoke it",
            "no, leave it",
        ),
        // `[y/N]`: blank declines.
        false,
    ) {
        window.notice("revoke declined; nothing changed")?;
        return Ok(1);
    }

    match newt_core::credential_registry::revoke_credential(
        &config_path,
        subject,
        handle,
        &root_key,
    ) {
        Ok(full) => {
            window.notice(&format!("revoked {full}"))?;
            Ok(0)
        }
        Err(error) => {
            window.notice(&format!("revoke failed: {error}"))?;
            Ok(1)
        }
    }
}

fn run_denials(journal: Option<PathBuf>, config: Option<&Path>) -> anyhow::Result<i32> {
    let config_path = config
        .map(Path::to_path_buf)
        .or_else(newt_core::Config::user_config_path)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the newt config path"))?;
    let path = journal.unwrap_or_else(|| config_path.with_file_name("denial-journal.jsonl"));
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "No denial journal at {}.\nConfined run_command refusals will be recorded there.",
                path.display()
            );
            return Ok(0);
        }
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    let head = newt_core::denial_journal::read_head(&path);
    print!("{}", render_denials(&body, &path, head.as_deref()));
    // The pre-chain migration, stated where an operator looking for missing
    // evidence will see it — the bytes were kept, not read.
    let pre_chain = newt_core::denial_journal::pre_chain_path(&path);
    if pre_chain.exists() {
        println!(
            "Records written before this journal adopted the chain were kept at\n  {}\n\
             They carry no integrity guarantee and are not read here.",
            pre_chain.display()
        );
    }
    Ok(0)
}

/// Resolve the capture-file path the same way the CLI arms it (newt-cli
/// `lib.rs`): an explicit `--capture`, else `$NEWT_FLIGHT_RECORDER` (unless it
/// is the `off`/`0` opt-out), else `<config dir>/flight-recorder/unconfined.jsonl`.
/// Thin env read over the pure [`capture_path_from`].
fn resolve_capture_path(capture: Option<PathBuf>, config_path: &Path) -> PathBuf {
    capture_path_from(capture, std::env::var_os(CAPTURE_PATH_ENV), config_path)
}

/// Pure capture-path selection (env value injected, so unit-testable): override
/// wins, then a non-opt-out `$NEWT_FLIGHT_RECORDER`, then the default beside the
/// config under `flight-recorder/`.
fn capture_path_from(
    capture: Option<PathBuf>,
    env_val: Option<std::ffi::OsString>,
    config_path: &Path,
) -> PathBuf {
    if let Some(p) = capture {
        return p;
    }
    if let Some(v) = env_val {
        let s = v.to_string_lossy();
        if !(s.eq_ignore_ascii_case("off") || s == "0") {
            return PathBuf::from(v);
        }
    }
    config_path
        .with_file_name("flight-recorder")
        .join("unconfined.jsonl")
}

fn run_propose(save: bool, capture: Option<PathBuf>, config: Option<&Path>) -> anyhow::Result<i32> {
    let config_path = config
        .map(Path::to_path_buf)
        .or_else(newt_core::Config::user_config_path)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the newt config path"))?;

    let capture_path = resolve_capture_path(capture, &config_path);
    let capture_text = match std::fs::read_to_string(&capture_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "No flight-recorder capture at {}.\n\
                 Run a task under `--full-access` first — every unconfined command records the \n\
                 authority a leash would have gated on, and `newt ocap propose` turns that into a \n\
                 reviewable policy.",
                capture_path.display()
            );
            return Ok(0);
        }
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", capture_path.display())),
    };

    // Read the four store files RAW (unverified) so already-written-but-unsigned
    // candidates count as accounted-for and re-runs converge (see ocap_propose).
    let store_dir = config_path.with_file_name("ocap");
    let store_files: Vec<(Verdict, Option<String>)> = VERDICTS
        .iter()
        .map(|&v| {
            (
                v,
                std::fs::read_to_string(store_dir.join(v.filename())).ok(),
            )
        })
        .collect();
    let approve_path = store_dir.join(Verdict::Approve.filename());
    let existing_approve = std::fs::read_to_string(&approve_path).ok();

    // newt-cli has no chrono dependency — use the newt-tui date helper (the
    // same one tuning_cmd uses). The pure planning core takes `now` as a string
    // so it stays wall-clock-free for tests.
    let now = newt_tui::probe::today_local_date();
    let (proposal, merged_toml) = plan_proposal(
        &capture_text,
        &store_files,
        existing_approve.as_deref(),
        newt_tui::ocap_high_danger_predicate(),
        &now,
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if proposal.is_empty() {
        println!(
            "The capture at {} is fully accounted for by the current policy — nothing to propose.",
            capture_path.display()
        );
        return Ok(0);
    }

    let wrote = save && proposal.additions_len() > 0;
    if wrote {
        if let Err(e) = std::fs::create_dir_all(&store_dir) {
            return Err(anyhow::anyhow!("create {}: {e}", store_dir.display()));
        }
        std::fs::write(&approve_path, &merged_toml)
            .map_err(|e| anyhow::anyhow!("write {}: {e}", approve_path.display()))?;
    }

    print!("{}", render(&proposal, wrote, &approve_path));
    Ok(0)
}

/// The pure planning core (mocked-unit testable): capture text + raw store file
/// contents + the existing `approve.toml` → the [`Proposal`] and the merged
/// `approve.toml` text the caller writes when `--write` is set.
///
/// Uses the UNVERIFIED `build_store` on purpose: an unsigned candidate already
/// on disk must be treated as accounted-for so re-running `propose` is
/// idempotent (the signed-load path would drop it and re-propose forever).
pub(crate) fn plan_proposal(
    capture_text: &str,
    store_files: &[(Verdict, Option<String>)],
    existing_approve: Option<&str>,
    is_high_danger: impl Fn(CapabilityClass, &str) -> bool,
    now: &str,
) -> Result<(Proposal, String), String> {
    let cap = read_capture_jsonl(capture_text);
    let (set, _warnings) = build_store(store_files);
    let in_policy = in_policy_pairs(&set);
    let proposal = propose_from_capture(&cap, &in_policy, is_high_danger, now);

    let mut merged = match existing_approve {
        Some(t) => PolicyFile::parse(t).map_err(|e| format!("approve.toml: {e}"))?,
        None => PolicyFile::default(),
    };
    merged.exec.extend(proposal.additions.exec.iter().cloned());
    merged.fs.extend(proposal.additions.fs.iter().cloned());
    merged.net.extend(proposal.additions.net.iter().cloned());
    let toml = merged
        .to_toml()
        .map_err(|e| format!("serialize approve.toml: {e}"))?;
    Ok((proposal, toml))
}

/// Render the operator-facing report (pure). Lists the proposed low-danger
/// candidates per class, the deferred high-danger observations with reasons,
/// and the next step (write, or bless).
pub(crate) fn render(proposal: &Proposal, wrote: bool, approve_path: &Path) -> String {
    let mut out = String::new();
    let a = &proposal.additions;

    if proposal.additions_len() > 0 {
        out.push_str(&format!(
            "Proposed {} durable candidate(s) from the flight recorder:\n",
            proposal.additions_len()
        ));
        for e in &a.exec {
            out.push_str(&format!("  exec  {}   [{}]\n", e.target, note_of(&e.note)));
        }
        for e in &a.fs {
            let mode = if e.write { "rw" } else { "ro" };
            out.push_str(&format!(
                "  fs    {} ({mode})   [{}]\n",
                e.path,
                note_of(&e.note)
            ));
        }
        for e in &a.net {
            out.push_str(&format!("  net   {}   [{}]\n", e.host, note_of(&e.note)));
        }
        out.push('\n');
    }

    if !proposal.deferred.is_empty() {
        out.push_str(&format!(
            "Deferred {} high-danger observation(s) — NOT proposed as durable grants:\n",
            proposal.deferred.len()
        ));
        for d in &proposal.deferred {
            out.push_str(&format!(
                "  {:<5} {} ({}x)\n        {}\n",
                d.class, d.target, d.count, d.reason
            ));
        }
        out.push('\n');
    }

    if wrote {
        out.push_str(&format!(
            "Wrote {} UNSIGNED candidate(s) to {}.\n\
             They do nothing until you bless them — review the file, then run:\n\
             \n    newt doctor --sign-ocap\n",
            proposal.additions_len(),
            approve_path.display()
        ));
    } else if proposal.additions_len() > 0 {
        out.push_str(
            "Dry run — nothing written. Re-run with `--save` to record these as \n\
             unsigned candidates, then bless them with `newt doctor --sign-ocap`.\n",
        );
    }
    out
}

fn note_of(note: &Option<String>) -> &str {
    note.as_deref().unwrap_or("observed")
}

/// Report what the chain proves about the journal before anything it contains
/// is presented as evidence.
///
/// This is the half that keeps the addressing honest: minting an id nothing
/// re-derives is not tamper-evidence. A clean chain says so too, including
/// which guarantee is missing when there is no head ref to anchor it.
fn render_integrity(breaks: &[ChainBreak], records: usize, anchored: bool) -> String {
    let anchor = if anchored {
        "anchored to the stored head"
    } else {
        "no stored head ref, so removal from the END was NOT checked"
    };
    if breaks.is_empty() {
        return format!("chain: {records} record(s) verified, {anchor}.\n\n");
    }

    let mut out = format!(
        "!! CHAIN BROKEN — this journal does not verify ({} problem(s); {anchor}).\n\
         !! The records below are still shown, and are NOT trustworthy evidence.\n",
        breaks.len()
    );
    for br in breaks {
        out.push_str(&match br {
            ChainBreak::Edited { index } => {
                format!("!!   record {index}: edited — its bytes no longer match its own address\n")
            }
            ChainBreak::Unreadable { index } => {
                format!("!!   record {index}: its address does not parse\n")
            }
            ChainBreak::BrokenLink { index } => format!(
                "!!   record {index}: broken link — a record before it was deleted or reordered\n"
            ),
            ChainBreak::NotAChain { index } => {
                format!("!!   record {index}: not a single-parent chain link\n")
            }
            ChainBreak::Truncated { expected_head } => format!(
                "!!   truncated — the chain no longer reaches the stored head {expected_head}\n"
            ),
        });
    }
    out.push('\n');
    out
}

/// Pure repair-oriented presentation of the denial journal.
///
/// `head` is the separately-stored head ref (the "one ref" of
/// *chain-plus-one-ref*); the fs read that produces it belongs to [`run_denials`],
/// which keeps this render pure.
pub(crate) fn render_denials(body: &str, path: &Path, head: Option<&str>) -> String {
    let lines = newt_core::denial_journal::read_jsonl(body);
    let breaks = newt_core::denial_journal::verify_chain(&lines, head);
    let summaries = newt_core::denial_journal::summarize(&lines);

    let mut out = render_integrity(&breaks, lines.len(), head.is_some());
    if summaries.is_empty() {
        out.push_str(&format!(
            "No structured denials recorded in {}.\n",
            path.display()
        ));
        return out;
    }

    out.push_str(&format!(
        "Denial repair journal: {} attempt(s), {} target(s)\nsource: {}\n\n",
        lines.len(),
        summaries.len(),
        path.display()
    ));
    for summary in summaries {
        out.push_str(&format!(
            "[{}] {}:{} ({}x)\n  reason: {}\n  replay: {}\n",
            summary.classification.as_str(),
            summary.kind,
            summary.target,
            summary.count,
            summary.reason,
            summary.example_command
        ));
        match summary.classification {
            newt_core::denial_journal::RepairClass::PolicyGap => out.push_str(
                "  next: review whether this target is necessary; if so, use the normal \
                 permission/policy approval path.\n",
            ),
            newt_core::denial_journal::RepairClass::Structural => out.push_str(
                "  next: implement or route around this shell construct; a grant cannot \
                 make the confined engine interpret it.\n",
            ),
            newt_core::denial_journal::RepairClass::GrantRetryFailure => out.push_str(
                "  next:\n  1. Reproduce from the replay command.\n  2. Do not add it to policy; \
                 the grant already failed.\n  3. Repair denial attribution, grant matching, \
                 or the upstream command implementation.\n",
            ),
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production-shaped danger predicate for the tests.
    fn danger(class: CapabilityClass, target: &str) -> bool {
        match class {
            CapabilityClass::Exec => matches!(target, "bash" | "python3" | "env"),
            CapabilityClass::Fs => target.starts_with("/home") || target == "/",
            CapabilityClass::Net => false,
        }
    }

    const CARGO: &str = r#"{"axis":"exec","target":"cargo","command":"cargo build","count":2}"#;
    const BASH: &str = r#"{"axis":"exec","target":"bash","command":"bash -c x","count":1}"#;

    #[test]
    fn plan_merges_low_danger_candidates_into_existing_approve() {
        // An existing signed grant is preserved; the new low-danger `cargo` is
        // appended unsigned; `bash` is deferred, not written.
        let existing = "[[exec]]\ntarget = \"git\"\nsig = \"ab\"\n";
        let capture = format!("{CARGO}\n{BASH}\n");
        let (proposal, merged) = plan_proposal(
            &capture,
            &[(Verdict::Approve, Some(existing.to_string()))],
            Some(existing),
            danger,
            "2026-07-17",
        )
        .unwrap();
        assert_eq!(proposal.additions.exec.len(), 1);
        assert_eq!(proposal.deferred.len(), 1);
        // Merged file keeps git (signed) + adds cargo (unsigned candidate).
        let parsed = PolicyFile::parse(&merged).unwrap();
        assert_eq!(parsed.exec.len(), 2);
        let git = parsed.exec.iter().find(|e| e.target == "git").unwrap();
        assert_eq!(git.sig.as_deref(), Some("ab"), "existing grant untouched");
        let cargo = parsed.exec.iter().find(|e| e.target == "cargo").unwrap();
        assert!(cargo.sig.is_none(), "candidate is unsigned");
        assert_eq!(cargo.by.as_deref(), Some("flight-recorder"));
    }

    #[test]
    fn plan_is_idempotent_against_an_already_written_unsigned_candidate() {
        // Re-running propose after a --write must NOT re-propose the candidate:
        // the unsigned entry already on disk counts as accounted-for (we read
        // the store UNVERIFIED for exactly this reason).
        let approve = "[[exec]]\ntarget = \"cargo\"\nby = \"flight-recorder\"\n";
        let (proposal, _merged) = plan_proposal(
            &format!("{CARGO}\n"),
            &[(Verdict::Approve, Some(approve.to_string()))],
            Some(approve),
            danger,
            "2026-07-17",
        )
        .unwrap();
        assert!(proposal.is_empty(), "no re-proposal: {proposal:?}");
    }

    #[test]
    fn render_dry_run_points_at_write_then_bless() {
        let (proposal, _) = plan_proposal(
            &format!("{CARGO}\n{BASH}\n"),
            &[],
            None,
            danger,
            "2026-07-17",
        )
        .unwrap();
        let path = PathBuf::from("/home/x/.newt/ocap/approve.toml");
        let dry = render(&proposal, false, &path);
        assert!(dry.contains("exec  cargo"));
        assert!(dry.contains("Deferred 1 high-danger"));
        assert!(dry.contains("bash"));
        assert!(dry.contains("--save"), "dry run tells you how to persist");
        assert!(!dry.contains("Wrote"));

        let wrote = render(&proposal, true, &path);
        assert!(wrote.contains("Wrote 1 UNSIGNED candidate"));
        assert!(wrote.contains("newt doctor --sign-ocap"));
    }

    #[test]
    fn capture_path_prefers_override_then_env_then_default() {
        let cfg = PathBuf::from("/home/x/.newt/config.toml");
        // Explicit override wins over everything.
        assert_eq!(
            capture_path_from(
                Some(PathBuf::from("/tmp/c.jsonl")),
                Some("/env/c".into()),
                &cfg
            ),
            PathBuf::from("/tmp/c.jsonl")
        );
        // A set (non-opt-out) env value is used.
        assert_eq!(
            capture_path_from(None, Some("/env/c.jsonl".into()), &cfg),
            PathBuf::from("/env/c.jsonl")
        );
        // The `off`/`0` opt-out falls through to the default.
        let off = capture_path_from(None, Some("off".into()), &cfg);
        assert!(off.ends_with("flight-recorder/unconfined.jsonl"), "{off:?}");
        // No override, no env → default beside the config.
        let def = capture_path_from(None, None, &cfg);
        assert!(def.ends_with("flight-recorder/unconfined.jsonl"), "{def:?}");
    }

    fn denial(command: &str) -> newt_core::denial_journal::DenialRecord {
        newt_core::denial_journal::DenialRecord {
            ts_claim: "2026-07-23T22:00:00Z".into(),
            command: command.into(),
            cwd: "/workspace".into(),
            stage: newt_core::denial_journal::DenialStage::AfterGrant,
            denials: vec![newt_core::denial_journal::JournalDenial {
                kind: "exec".into(),
                target: "confinement".into(),
                reason: "exec of \"confinement\" is not within the granted authority".into(),
            }],
        }
    }

    /// A chained journal body plus the head ref that would sit beside it.
    fn journal_body(commands: &[&str]) -> (String, String) {
        let mut journal = newt_core::event_journal::Journal::new();
        let body = commands
            .iter()
            .map(|c| {
                journal
                    .append(denial(c))
                    .expect("append")
                    .render_line()
                    .expect("render")
            })
            .collect::<Vec<_>>()
            .join("\n");
        (body, journal.head().expect("head").to_string())
    }

    const JOURNAL: &str = "/home/x/.newt/denial-journal.jsonl";

    #[test]
    fn denial_report_presents_repair_class_and_replay_fixture() {
        let (body, head) = journal_body(&["wc -l src/lib.rs"]);
        let report = render_denials(&body, Path::new(JOURNAL), Some(&head));

        assert!(report.contains("grant-retry"));
        assert!(report.contains("exec:confinement"));
        assert!(report.contains("2. Do not add it to policy"));
        assert!(report.contains("wc -l src/lib.rs"));
        assert!(report.contains(JOURNAL));
        assert!(
            report.contains("1 record(s) verified, anchored to the stored head"),
            "an intact journal must SAY it verified: {report}"
        );
    }

    /// The reader is the point. A journal with a record removed must be
    /// reported as broken evidence, not folded into a tidy repair summary.
    #[test]
    fn a_journal_with_a_deleted_record_is_reported_as_broken() {
        let (body, head) = journal_body(&["wc -l a.rs", "wc -l b.rs", "wc -l c.rs"]);
        let kept: Vec<&str> = body
            .lines()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, l)| l)
            .collect();
        let report = render_denials(&kept.join("\n"), Path::new(JOURNAL), Some(&head));

        assert!(report.contains("CHAIN BROKEN"), "{report}");
        assert!(
            report.contains("record 1: broken link"),
            "the break must name the record it was found at: {report}"
        );
        // The surviving evidence is still shown — unusable evidence is not the
        // same as no evidence.
        assert!(report.contains("wc -l a.rs"));
    }

    /// Truncation is only visible against the stored head, and the report says
    /// which guarantee it had when there is none.
    #[test]
    fn a_truncated_journal_needs_the_head_ref_to_be_seen() {
        let (body, head) = journal_body(&["wc -l a.rs", "wc -l b.rs"]);
        let first = body.lines().next().unwrap();

        let unanchored = render_denials(first, Path::new(JOURNAL), None);
        assert!(!unanchored.contains("CHAIN BROKEN"));
        assert!(
            unanchored.contains("removal from the END was NOT checked"),
            "the missing guarantee must be stated, not implied: {unanchored}"
        );

        let anchored = render_denials(first, Path::new(JOURNAL), Some(&head));
        assert!(anchored.contains("truncated"), "{anchored}");
    }

    /// A journal emptied of every record is the tamper a per-record address
    /// cannot see at all: there is nothing left to check the address of.
    #[test]
    fn an_emptied_journal_is_reported_against_the_head_ref() {
        let (_, head) = journal_body(&["wc -l a.rs"]);
        let report = render_denials("", Path::new(JOURNAL), Some(&head));
        assert!(report.contains("CHAIN BROKEN"), "{report}");
        assert!(
            report.contains("No structured denials recorded"),
            "{report}"
        );
    }
}
