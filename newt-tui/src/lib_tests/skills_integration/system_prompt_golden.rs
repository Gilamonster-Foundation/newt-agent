//! #2933 PR1: the assembled system prompt, pinned byte for byte.
//!
//! The goldens in `newt-core/tests/golden/system_prompt/` were captured from
//! `build_system_prompt_with_persona` BEFORE its pure part was extracted into
//! `newt_core::assemble_system_prompt`. This test keeps the real builder (config
//! resolution, directory listing, README, skills) on those bytes; the core
//! test `system_prompt_golden` keeps the pure function on the same files.
//! `NEWT_GOLDEN_UPDATE=1` rewrites them, for a deliberate change only.

use super::system_prompt::{write_skill, SkillRootSandbox};
use super::*;

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("newt-core")
        .join("tests")
        .join("golden")
        .join("system_prompt")
        .join(format!("{name}.golden"))
}

/// One variant: the config the operator wrote, the soul, and the persona.
fn assembled(config: &str, soul: &str, persona: Option<&Persona>) -> String {
    let root = tempfile::TempDir::new().unwrap();
    let ws = root.path().join("ws");
    let config_dir = root.path().join("config");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(ws.join(".git")).unwrap();
    fs::write(ws.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(ws.join("README.md"), "# Fixture\nA tiny project.\n").unwrap();
    write_skill(&config_dir.join("skills"), "fixture-skill", "A fixture skill");
    fs::write(config_dir.join("config.toml"), config).unwrap();
    let _sandbox = SkillRootSandbox::enter(&config_dir, &ws);
    let ws_str = ws.to_str().unwrap();
    build_system_prompt_with_persona(
        ws_str,
        soul,
        Some("## AGENTS.md\nUse four spaces."),
        persona,
        "plan.md",
        None,
    )
    .replace(ws_str, "/ws")
}

fn check(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var("NEWT_GOLDEN_UPDATE").as_deref() == Ok("1") {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("golden {name} missing; capture it with NEWT_GOLDEN_UPDATE=1"));
    assert_eq!(expected, actual, "system prompt golden `{name}` drifted");
}

fn persona(name: &str, coach: bool) -> Persona {
    let mut p = test_persona(
        name,
        "Persona body.",
        std::path::PathBuf::from("/personas/p.md"),
    );
    if coach {
        p.profile.altitude = Some(newt_core::Altitude::Coach);
    }
    p
}

const FOUR_TOOLS: &str = "[tool_exposure]\nallow_list = [\"read_file\", \"write_file\", \"edit_file\", \"run_command\"]\n";

#[serial_test::serial(real_fs)]
#[test]
fn goldens_pin_the_assembled_prompt() {
    let doer = newt_core::DEFAULT_SOUL;
    check("doer", &assembled("", doer, None));
    check("coach", &assembled("", doer, Some(&persona("coach", true))));
    check("persona", &assembled("", doer, Some(&persona("reviewer", false))));
    check(
        "read_only",
        &assembled("[tool_exposure]\nread_only = true\n", doer, None),
    );
    check("allow_list_four", &assembled(FOUR_TOOLS, doer, None));
    check(
        "narration",
        &assembled("[tui]\nnarration_intent_line = true\n", doer, None),
    );
    check(
        "custom_soul",
        &assembled("", "You are a Django expert.", None),
    );
}
