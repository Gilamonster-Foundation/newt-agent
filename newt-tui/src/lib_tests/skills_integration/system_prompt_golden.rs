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
fn assembled(config: &str, persona: Option<&Persona>) -> String {
    assembled_reporting(config, persona, None)
}

fn assembled_reporting(
    config: &str,
    persona: Option<&Persona>,
    sink: Option<&newt_core::agentic::request_report::ReportSink>,
) -> String {
    let root = tempfile::TempDir::new().unwrap();
    let ws = root.path().join("ws");
    let config_dir = root.path().join("config");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(ws.join(".git")).unwrap();
    fs::write(ws.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(ws.join("README.md"), "# Fixture\nA tiny project.\n").unwrap();
    write_skill(
        &config_dir.join("skills"),
        "fixture-skill",
        "A fixture skill",
    );
    fs::write(config_dir.join("config.toml"), config).unwrap();
    // The sandbox rewrites HOME, NEWT_CONFIG* and the cwd: hold the process-environment
    // lock (the one the settings and workspace tests take) from before the first
    // write until after the restore. Declared first, so it drops last.
    let _env = newt_core::process_env::lock();
    let _sandbox = SkillRootSandbox::enter(&config_dir, &ws);
    let ws_str = ws.to_str().unwrap();
    build_system_prompt_with_persona(
        ws_str,
        Some("## AGENTS.md\nUse four spaces."),
        persona,
        "plan.md",
        sink,
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
    check("doer", &assembled("", None));
    check("coach", &assembled("", Some(&persona("coach", true))));
    check("persona", &assembled("", Some(&persona("reviewer", false))));
    check(
        "read_only",
        &assembled("[tool_exposure]\nread_only = true\n", None),
    );
    check("allow_list_four", &assembled(FOUR_TOOLS, None));
    check(
        "narration",
        &assembled("[tui]\nnarration_intent_line = true\n", None),
    );
    // #2933 PR6: preset `pi` over the pi tool list, through the real builder.
    check(
        "pi",
        &assembled(
            "[tool_exposure]\nallow_list = [\"read_file\", \"write_file\", \"edit_file\", \"run_command\"]\n[prompt]\npreset = \"pi\"\n",
            Some(&persona("reviewer", false)),
        ),
    );
}

/// #2933 PR3: `[prompt] components.off = ["three-cs"]` removes that paragraph and
/// nothing else from the assembled prompt.
#[serial_test::serial(real_fs)]
#[test]
fn switching_a_component_off_drops_exactly_its_paragraph() {
    let base = assembled("", None);
    let off = assembled("[prompt]\ncomponents = { off = [\"three-cs\"] }\n", None);
    let start = base.find("**Working code first").unwrap();
    let end = base.find("\n\n## AGENTS.md").unwrap_or_else(|| {
        panic!("project context follows the soul:\n{base}");
    });
    let expected = format!("{}{}", base[..start].trim_end_matches('\n'), &base[end..]);
    assert_eq!(off, expected);
}

/// #2933 PR4: `/context request` names the soul by component id, and a component
/// switched off is gone from the report as it is from the prompt.
#[serial_test::serial(real_fs)]
#[test]
fn the_request_report_names_components_by_id_with_nothing_unattributed() {
    let labels = |config: &str| {
        let sink = newt_core::agentic::request_report::ReportSink::new();
        // The soul and context providers label their own blocks in production,
        // the soul as ONE block (which must not swallow its components).
        sink.annotate("soul", "test", newt_core::DEFAULT_SOUL);
        sink.annotate("agents", "test", "## AGENTS.md\nUse four spaces.");
        let prompt = assembled_reporting(config, None, Some(&sink));
        sink.arm_capture_next();
        let _ = sink.observe_request(&[&prompt], None);
        let report = sink.last_report().expect("report captured");
        report.system[0]
            .components
            .iter()
            .map(|c| c.label.clone())
            .collect::<Vec<_>>()
    };
    let all = labels("");
    for id in [
        "identity",
        "tools",
        "disposition",
        "habit-one-change",
        "three-cs",
    ] {
        assert!(all.iter().any(|l| l == id), "{id} missing from {all:?}");
    }
    assert!(!all.iter().any(|l| l == "soul"), "{all:?}");
    // The one gap is the `Workspace:` header, which the memory provider labels in
    // production (`registered_agents_provider_block_reaches_prompt`); no soul byte.
    let gaps = |labels: &[String]| labels.iter().filter(|l| l.contains("unattributed")).count();
    assert_eq!(gaps(&all), 1, "{all:?}");
    let off = labels("[prompt]\ncomponents = { off = [\"three-cs\"] }\n");
    assert!(!off.iter().any(|l| l == "three-cs"), "{off:?}");
    assert_eq!(gaps(&off), 1, "{off:?}");
}

/// #2990 round 2: the prompt follows the session's last ACCEPTED config. Accept a
/// newer config through the chat loop's `reresolve`, break the file, then `/new`:
/// the rebuilt prompt renders from the newer config, not from the first one nor
/// from defaults.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn a_failed_refresh_then_new_renders_from_the_newer_accepted_config() {
    let root = tempfile::TempDir::new().unwrap();
    let ws = root.path().join("ws");
    let config_dir = root.path().join("config");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    let _env = newt_core::process_env::lock();
    let _sandbox = SkillRootSandbox::enter(&config_dir, &ws);

    let first = "[tool_exposure]\nallow_list = [\"read_file\", \"write_file\", \"edit_file\"]\n[prompt]\ncomponents = { off = [\"three-cs\"] }\n";
    let newer = "[tool_exposure]\nallow_list = [\"read_file\"]\n";
    let broken = "[tool_exposure]\nallow_list = [\"read_file\"]\n[prompt]\ncomponents = { off = [\"nope\"] }\n";

    fs::write(&config, first).unwrap();
    let mut cfg = crate::resolve_runtime_or_default();
    fs::write(&config, newer).unwrap();
    crate::chat::reresolve(&mut cfg, false, false);
    fs::write(&config, broken).unwrap();
    crate::chat::reresolve(&mut cfg, false, false);

    let mut memory = newt_core::MemoryManager::new();
    memory
        .initialize_all(&newt_core::SessionContext {
            workspace: ws.to_string_lossy().into_owned(),
            session_id: "s".into(),
        })
        .await;
    let mut system = String::new();
    let mut conversation_id = String::from("s");
    let mode_states = ConversationModeStates::default();
    let mut ctx = ConversationResetContext {
        cfg: &cfg,
        memory: &mut memory,
        system: &mut system,
        conversation_id: &mut conversation_id,
        mode_states: &mode_states,
        sink: None,
    };
    reset_conversation(ws.to_str().unwrap(), None, &mut ctx);

    assert!(
        system.contains("You have tools: read_file."),
        "the newer allow list names one tool:\n{system}"
    );
    assert!(
        system.contains("Working code first"),
        "the newer config has three-cs on:\n{system}"
    );
}
