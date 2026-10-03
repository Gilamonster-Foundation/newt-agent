//! `/ocap propose` in the TUI (#2679), round-2 review findings on #2687.

use super::{ocap_propose_command_lines, parse_ocap_command};

const S1: &str = r#"{"axis":"exec","target":"jq","command":"jq .","count":1,"session":"s1"}"#;
const FOREIGN: &str =
    r#"{"axis":"exec","target":"crontab","command":"crontab -l","count":1,"session":"s2"}"#;
const LEGACY: &str = r#"{"axis":"exec","target":"curl","command":"curl x","count":1}"#;

/// A config dir with a capture file, plus an existing approve.toml.
fn fixture(approve: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let capture = dir.path().join("capture.jsonl");
    std::fs::write(&capture, format!("{S1}\n{FOREIGN}\n{LEGACY}\n")).unwrap();
    if let Some(text) = approve {
        std::fs::create_dir_all(dir.path().join("ocap")).unwrap();
        std::fs::write(dir.path().join("ocap/approve.toml"), text).unwrap();
    }
    (dir, config, capture)
}

/// P1: saving MERGES into approve.toml. An existing signed entry and its
/// signature survive, and the new candidate is appended. The round-1 code
/// passed `existing: None` and overwrote the file, losing `rg`.
#[test]
fn save_keeps_existing_approvals_and_their_signatures() {
    let existing = "[[exec]]\ntarget = \"rg\"\nsig = \"deadbeef\"\n";
    let (dir, config, capture) = fixture(Some(existing));
    ocap_propose_command_lines(Some(capture), Some(&config), true, Some("s1")).unwrap();
    let saved = std::fs::read_to_string(dir.path().join("ocap/approve.toml")).unwrap();
    assert!(
        saved.contains("\"rg\"") && saved.contains("deadbeef"),
        "existing entry lost:\n{saved}"
    );
    assert!(saved.contains("\"jq\""), "new candidate missing:\n{saved}");
}

/// P1: an approve.toml that can't be parsed refuses the save rather than
/// overwrite it.
#[test]
fn an_unparseable_approve_file_is_never_overwritten() {
    let (dir, config, capture) = fixture(Some("not [valid toml"));
    let out = ocap_propose_command_lines(Some(capture), Some(&config), true, Some("s1"));
    assert!(out.is_err(), "expected a refusal, got {out:?}");
    let left = std::fs::read_to_string(dir.path().join("ocap/approve.toml")).unwrap();
    assert_eq!(left, "not [valid toml");
}

/// P2: only this session's caveats are proposed (foreign and legacy rows are
/// not), and with no owning session nothing is proposed or written at all.
#[test]
fn scope_is_this_session_only_and_no_session_proposes_nothing() {
    let (_dir, config, capture) = fixture(None);
    let lines = ocap_propose_command_lines(Some(capture.clone()), Some(&config), false, Some("s1"))
        .unwrap()
        .join("\n");
    assert!(lines.contains("jq"), "{lines}");
    assert!(
        !lines.contains("crontab") && !lines.contains("curl"),
        "{lines}"
    );

    let (dir, config, capture) = fixture(None);
    let lines = ocap_propose_command_lines(Some(capture), Some(&config), true, None)
        .unwrap()
        .join("\n");
    assert!(lines.contains("No active session"), "{lines}");
    assert!(
        !dir.path().join("ocap/approve.toml").exists(),
        "nothing may be written"
    );
}

/// P2: the advertised grammar parses; anything else is rejected.
#[test]
fn ocap_grammar_is_exact() {
    assert_eq!(parse_ocap_command("ocap propose"), Some(false));
    assert_eq!(parse_ocap_command("ocap propose save"), Some(true));
    assert_eq!(parse_ocap_command("ocap  propose   save"), Some(true));
    assert_eq!(parse_ocap_command("ocap save"), None);
    assert_eq!(parse_ocap_command("ocap"), None);
    assert_eq!(parse_ocap_command("ocap propose now"), None);
}
