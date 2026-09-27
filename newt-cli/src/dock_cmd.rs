//! `newt dock` — the approved-dock management surface (newt-web mesh docking,
//! requirement 5 / Phase 3).
//!
//! A dock is the operator's standing, revocable approval for another of their
//! own agents to surface its sessions into a hub cockpit over agent-mesh. The
//! mesh handshake already proves same-operator; these commands record *which*
//! peer the operator meant, signed by the root key so the hub can refuse an
//! unapproved peer and drop a revoked one.
//!
//! Every write is root-key-gated and terminal-confirmed exactly like
//! `newt ocap revoke-credential`: a headless session cannot approve or revoke,
//! because it cannot be asked. `approve` additionally derives the 6-word SAS
//! from the peer's pubkey so the operator confirms the exact key, not a label.

use std::path::{Path, PathBuf};

use clap::Subcommand;

use newt_core::dock_registry;

/// Least authority by default: a dock may only mirror (read) unless the operator
/// explicitly asks for inject. Widening to `mirror-inject` is a deliberate choice
/// on the command line, never the default.
const DEFAULT_SCOPE: &str = "mirror";

#[derive(Subcommand, Debug)]
pub enum DockCmd {
    /// Approve a peer agent to dock into this hub. Shows the peer pubkey's 6-word
    /// mnemonic — the SAME words the peer's own newt-web prints when it binds its
    /// dock service — for you to compare, then (on confirmation) writes a signed
    /// approval to `~/.newt/ocap/docks.d/peers.toml`.
    ///
    /// `--staged` promotes a host that uplinked to this hub and was staged
    /// (see `newt dock list`), taking its key and name from the staged entry.
    Approve {
        /// The peer's mesh agent public key, 64 hex chars (the `pubkey` the
        /// peer's `newt-web` prints when it binds its dock service).
        #[arg(long, value_name = "HEX", required_unless_present = "staged")]
        pubkey: Option<String>,
        /// A staged host's agent fingerprint, or any unambiguous prefix of one.
        #[arg(long, value_name = "PEER-SHORT", conflicts_with = "pubkey")]
        staged: Option<String>,
        /// Operator-facing label for the peer (e.g. `laptop-b`). Defaults to a
        /// staged host's instance name.
        #[arg(long, value_name = "LABEL", required_unless_present = "staged")]
        label: Option<String>,
        /// Approval scope: `mirror` (read only, the default) or `mirror-inject`
        /// (also enqueue prompts, D2). Least authority by default.
        #[arg(long, default_value = DEFAULT_SCOPE)]
        scope: String,
        /// Operator root key. Default: `~/.newt/identity.pem`.
        #[arg(long, env = "NEWT_OPERATOR_KEY", value_name = "FILE")]
        operator_key_path: Option<PathBuf>,
    },
    /// Revoke one approved dock by peer-fingerprint prefix. The row is flagged
    /// and re-signed (not deleted) and its generation bumps, so a live dock is
    /// dropped at the responder's next re-check, not merely refused next time.
    Revoke {
        /// Peer agent fingerprint, or any unambiguous prefix of one.
        #[arg(value_name = "PEER-SHORT")]
        peer: String,
        /// Operator root key. Default: `~/.newt/identity.pem`.
        #[arg(long, env = "NEWT_OPERATOR_KEY", value_name = "FILE")]
        operator_key_path: Option<PathBuf>,
    },
    /// Revoke EVERY live dock atomically — the `/undock all` kill-switch.
    RevokeAll {
        /// Operator root key. Default: `~/.newt/identity.pem`.
        #[arg(long, env = "NEWT_OPERATOR_KEY", value_name = "FILE")]
        operator_key_path: Option<PathBuf>,
    },
    /// List the live (approved, non-revoked) docks. Read-only, no terminal gate.
    List {
        /// Operator root key. Default: `~/.newt/identity.pem`.
        #[arg(long, env = "NEWT_OPERATOR_KEY", value_name = "FILE")]
        operator_key_path: Option<PathBuf>,
    },
}

/// Dispatch `newt dock <cmd>`.
pub fn run(cmd: DockCmd, config: Option<&Path>) -> anyhow::Result<i32> {
    match cmd {
        DockCmd::Approve {
            pubkey,
            staged,
            label,
            scope,
            operator_key_path,
        } => {
            let (pubkey, label, pairing) = match staged {
                Some(prefix) => {
                    let host = staged_host(&config_path(config)?, &prefix)?;
                    let Some(pairing) = host.pairing() else {
                        anyhow::bail!(
                            "host `{}` has not completed pairing: run `newt-mesh dock` on it",
                            host.peer_label
                        );
                    };
                    (
                        host.peer_pubkey,
                        label.unwrap_or(host.peer_label),
                        Some(pairing),
                    )
                }
                None => (pubkey.unwrap_or_default(), label.unwrap_or_default(), None),
            };
            run_approve(
                &pubkey,
                &label,
                &scope,
                pairing.as_ref(),
                operator_key_path,
                config,
            )
        }
        DockCmd::Revoke {
            peer,
            operator_key_path,
        } => run_revoke(&peer, operator_key_path, config),
        DockCmd::RevokeAll { operator_key_path } => run_revoke_all(operator_key_path, config),
        DockCmd::List { operator_key_path } => run_list(operator_key_path, config),
    }
}

/// The one live staged host whose fingerprint starts with `prefix`.
fn staged_host(config_path: &Path, prefix: &str) -> anyhow::Result<dock_registry::StagedHost> {
    let mut matches: Vec<_> =
        dock_registry::staged_hosts(config_path, std::time::SystemTime::now())
            .into_iter()
            .filter(|host| !prefix.is_empty() && host.peer_agent_fingerprint.starts_with(prefix))
            .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => anyhow::bail!("no staged host matches `{prefix}` (see `newt dock list`)"),
        n => anyhow::bail!("`{prefix}` matches {n} staged hosts; use a longer prefix"),
    }
}

/// Whether the staged host `peer_fp` still holds `pairing`: the host may pair
/// afresh while the operator compares codes, and only the code compared may
/// be approved.
fn pairing_still_held(config_path: &Path, peer_fp: &str, pairing: &dock_registry::Pairing) -> bool {
    staged_host(config_path, peer_fp).is_ok_and(|host| host.pairing().as_ref() == Some(pairing))
}

fn config_path(config: Option<&Path>) -> anyhow::Result<PathBuf> {
    config
        .map(Path::to_path_buf)
        .or_else(newt_core::Config::user_config_path)
        .ok_or_else(|| anyhow::anyhow!("cannot locate the newt config directory"))
}

fn load_root(operator_key_path: Option<PathBuf>) -> anyhow::Result<newt_identity::UserKey> {
    let key_path = match operator_key_path {
        Some(path) => path,
        None => newt_identity::default_key_path()?,
    };
    Ok(newt_identity::load_user_key(&key_path)?)
}

/// Parse 64 lowercase/uppercase hex chars into a 32-byte agent public key.
fn parse_pubkey(hex: &str) -> anyhow::Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        anyhow::bail!("pubkey must be 64 hex chars (32 bytes), got {}", hex.len());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow::anyhow!("pubkey is not valid hex"))?;
    }
    Ok(out)
}

/// `newt dock approve` — derive + show the SAS, confirm at the terminal, then
/// write the signed approval.
fn run_approve(
    pubkey_hex: &str,
    label: &str,
    scope: &str,
    pairing: Option<&dock_registry::Pairing>,
    operator_key_path: Option<PathBuf>,
    config: Option<&Path>,
) -> anyhow::Result<i32> {
    let config_path = config_path(config)?;
    let root_key = load_root(operator_key_path)?;
    let pubkey = parse_pubkey(pubkey_hex)?;
    let scope = dock_registry::DockScope::parse(scope)?;

    let issuer = root_key.public().fingerprint().hex();
    let peer_fp = dock_registry::agent_fingerprint_of_pubkey(&pubkey);
    // Fresh per-ceremony secrets so the SAS words are unpredictable and bound to
    // this approval. The unguessable id generator is the same randomness source
    // the CSP nonces trust — no second RNG dependency.
    let nonce = newt_core::new_conversation_id();
    let blinding = newt_core::new_conversation_id();
    let ceremony = dock_registry::dock_ceremony(
        &issuer,
        label,
        &pubkey,
        nonce.as_bytes(),
        blinding.as_bytes(),
    );

    // Terminal-gated (the capability, not a flag): a session with no terminal
    // cannot obtain the window and falls through to the decline arm.
    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    window.notice(&format!(
        "dock approval for `{label}`\n  peer agent fp : {}\n  peer pubkey   : {}…\n  key words     : {}\nThe peer's newt-web prints these SAME words when it binds its dock service —\nconfirm they match before approving (a cross-check of the peer's key, not a\ntwo-party anti-MITM SAS: same-operator trust is already proven by the mesh handshake).",
        &peer_fp[..16.min(peer_fp.len())],
        &pubkey_hex[..16.min(pubkey_hex.len())],
        ceremony.sas_words.join(" "),
    ))?;
    if let Some(pairing) = pairing {
        window.notice(&format!(
            "  pairing code  : {}\nThe host's `newt-mesh dock` shows this SAME code for this pairing. \
             Approve only if they match.",
            pairing.code
        ))?;
    }
    if !newt_core::interaction_terminal::confirmed_on_terminal(
        &window,
        &newt_core::interaction_form::confirm(
            format!("approve dock with `{label}`?"),
            "",
            "yes, approve it",
            "no, leave it",
        ),
        // `[y/N]`: blank declines.
        false,
    ) {
        window.notice("dock approval declined; nothing changed")?;
        return Ok(1);
    }
    if pairing.is_some_and(|p| !pairing_still_held(&config_path, &peer_fp, p)) {
        window.notice("the host paired again while you compared codes; nothing changed")?;
        return Ok(1);
    }

    match dock_registry::approve_dock(
        &config_path,
        &peer_fp,
        label,
        pubkey_hex.trim(),
        scope,
        // A paired host's approval commits to the pairing transcript.
        pairing.map_or(&ceremony.transcript_id, |p| &p.transcript_id),
        &root_key,
    ) {
        Ok(path) => {
            window.notice(&format!(
                "approved `{label}` ({}) → {}",
                &peer_fp[..16.min(peer_fp.len())],
                path.display()
            ))?;
            if let Err(error) = dock_registry::unstage_host(&config_path, &peer_fp) {
                window.notice(&format!("(could not clear its staged entry: {error})"))?;
            }
            Ok(0)
        }
        Err(error) => {
            window.notice(&format!("dock approval failed: {error}"))?;
            Ok(1)
        }
    }
}

fn run_revoke(
    peer: &str,
    operator_key_path: Option<PathBuf>,
    config: Option<&Path>,
) -> anyhow::Result<i32> {
    let config_path = config_path(config)?;
    let root_key = load_root(operator_key_path)?;

    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    if !newt_core::interaction_terminal::confirmed_on_terminal(
        &window,
        &newt_core::interaction_form::confirm(
            format!("revoke the dock for `{peer}`?"),
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
    match dock_registry::revoke_dock(&config_path, peer, &root_key) {
        Ok(full) => {
            window.notice(&format!("revoked dock {full}"))?;
            Ok(0)
        }
        Err(error) => {
            window.notice(&format!("revoke failed: {error}"))?;
            Ok(1)
        }
    }
}

fn run_revoke_all(
    operator_key_path: Option<PathBuf>,
    config: Option<&Path>,
) -> anyhow::Result<i32> {
    let config_path = config_path(config)?;
    let root_key = load_root(operator_key_path)?;

    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    if !newt_core::interaction_terminal::confirmed_on_terminal(
        &window,
        &newt_core::interaction_form::confirm(
            "revoke ALL live docks?",
            "",
            "yes, revoke them all",
            "no, leave them",
        ),
        // `[y/N]`: blank declines.
        false,
    ) {
        window.notice("revoke-all declined; nothing changed")?;
        return Ok(1);
    }
    match dock_registry::revoke_all_docks(&config_path, &root_key) {
        Ok(revoked) => {
            window.notice(&format!("revoked {} dock(s)", revoked.len()))?;
            Ok(0)
        }
        Err(error) => {
            window.notice(&format!("revoke-all failed: {error}"))?;
            Ok(1)
        }
    }
}

fn run_list(operator_key_path: Option<PathBuf>, config: Option<&Path>) -> anyhow::Result<i32> {
    let config_path = config_path(config)?;
    let root_key = load_root(operator_key_path)?;
    let (registry, warnings) = dock_registry::load_docks(&config_path, Some(&root_key.public()));
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    let staged = dock_registry::staged_hosts(&config_path, std::time::SystemTime::now());
    if !staged.is_empty() {
        println!("staged hosts (promote with `newt dock approve --staged <PEER-FP>`):");
        let rows: Vec<[String; 3]> = staged
            .iter()
            .map(|host| {
                [
                    host.peer_label.clone(),
                    host.peer_agent_fingerprint[..16].to_string(),
                    "staged".to_string(),
                ]
            })
            .collect();
        print!("{}", dock_table(&rows));
    }
    let docks = registry.live();
    if docks.is_empty() {
        println!("no approved docks");
        return Ok(0);
    }
    let rows: Vec<[String; 3]> = docks
        .iter()
        .map(|dock| {
            [
                dock.peer_label.clone(),
                dock.peer_agent_fingerprint[..16.min(dock.peer_agent_fingerprint.len())]
                    .to_string(),
                dock.scope.as_wire().to_string(),
            ]
        })
        .collect();
    print!("{}", dock_table(&rows));
    Ok(0)
}

/// Render the `newt dock list` listing.
///
/// Extracted so the exact bytes are testable without a dock registry (#1916).
/// Byte-identical to the `println!`s it replaces.
fn dock_table(rows: &[[String; 3]]) -> String {
    use newt_core::markup::table::{render_table, Column};
    let columns = [
        Column::new("LABEL"),
        Column::new("PEER-FP"),
        Column::new("scope"),
    ];
    let data: Vec<Vec<String>> = rows.iter().map(|r| r.to_vec()).collect();
    render_table(&columns, &data)
}

#[cfg(test)]
mod d3c {
    /// **The byte golden for `newt dock list` as it ships today** (#1916).
    /// Captured from the shipping renderer — see `models_cmd::d3c`.
    #[test]
    fn the_dock_listing_is_byte_exact() {
        let rows = [[
            "laptop-b".to_string(),
            "abcdef0123456789".to_string(),
            "mirror-inject".to_string(),
        ]];
        assert_eq!(
            super::dock_table(&rows),
            concat!(
                "| LABEL    | PEER-FP          | scope         |\n",
                "| -------- | ---------------- | ------------- |\n",
                "| laptop-b | abcdef0123456789 | mirror-inject |\n",
            )
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pubkey_round_trips_and_rejects_bad_input() {
        let hex = "aa".repeat(32);
        assert_eq!(parse_pubkey(&hex).unwrap(), [0xaau8; 32]);
        assert!(parse_pubkey("short").is_err());
        assert!(parse_pubkey(&"zz".repeat(32)).is_err());
    }

    /// Only the pairing the operator compared may be approved: once the host
    /// pairs afresh, or its staging lapses, the compared one is not held (#2616).
    #[test]
    fn only_the_pairing_still_held_may_be_approved() {
        use dock_registry::{pair_staged_host, pairing_commitment, PairingStep};
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let (host, hub, now) = ([1; 32], [9; 32], std::time::SystemTime::now());
        let fp = dock_registry::agent_fingerprint_of_pubkey(&host);
        let pair = |nonce: [u8; 16]| {
            let commit = PairingStep::Commit(pairing_commitment(&hub, &host, &nonce));
            pair_staged_host(&config, &host, &hub, commit, [7; 16], now).unwrap();
            pair_staged_host(
                &config,
                &host,
                &hub,
                PairingStep::Reveal(nonce),
                [7; 16],
                now,
            )
            .unwrap();
            staged_host(&config, &fp).unwrap().pairing().unwrap()
        };
        dock_registry::stage_host(&config, &host, "nuc1", &hub, now).unwrap();
        let compared = pair([5; 16]);
        assert!(pairing_still_held(&config, &fp, &compared));
        pair([6; 16]);
        assert!(!pairing_still_held(&config, &fp, &compared), "re-paired");
        dock_registry::unstage_host(&config, &fp).unwrap();
        assert!(!pairing_still_held(&config, &fp, &compared), "unstaged");
    }

    #[test]
    fn staged_resolves_one_unambiguous_live_host() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let now = std::time::SystemTime::now();
        dock_registry::stage_host(&config, &[1; 32], "nuc1", &[9; 32], now).unwrap();
        dock_registry::stage_host(&config, &[2; 32], "nuc2", &[9; 32], now).unwrap();
        let nuc1 = dock_registry::agent_fingerprint_of_pubkey(&[1; 32]);

        let host = staged_host(&config, &nuc1[..12]).unwrap();
        assert_eq!(
            (host.peer_label.as_str(), host.peer_pubkey),
            ("nuc1", "01".repeat(32))
        );
        assert!(
            staged_host(&config, "").is_err(),
            "an empty prefix names nothing"
        );
        assert!(staged_host(&config, "zz").is_err(), "no match");

        let twin = (3..=u8::MAX)
            .map(|b| [b; 32])
            .find(|k| dock_registry::agent_fingerprint_of_pubkey(k)[..1] == nuc1[..1])
            .unwrap();
        dock_registry::stage_host(&config, &twin, "twin", &[9; 32], now).unwrap();
        assert!(
            staged_host(&config, &nuc1[..1]).is_err(),
            "a prefix two hosts share is ambiguous"
        );
    }

    #[test]
    fn approve_takes_either_a_pubkey_and_label_or_a_staged_host() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            cmd: DockCmd,
        }
        let parse = |args: &[&str]| Cli::try_parse_from([&["newt"], args].concat()).map(|c| c.cmd);
        assert!(matches!(
            parse(&["approve", "--staged", "ab12"]),
            Ok(DockCmd::Approve {
                staged: Some(_),
                pubkey: None,
                label: None,
                ..
            })
        ));
        assert!(parse(&["approve", "--pubkey", "aa", "--label", "x"]).is_ok());
        assert!(
            parse(&["approve", "--label", "x"]).is_err(),
            "no key and no staged host"
        );
        assert!(parse(&["approve", "--staged", "ab", "--pubkey", "aa"]).is_err());
    }
}
