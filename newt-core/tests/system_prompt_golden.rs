//! #2933 PR1: `assemble_system_prompt` reproduces, byte for byte, the goldens
//! captured from the builder it was extracted from. Mocked: pure inputs only.
//! `NEWT_GOLDEN_UPDATE=1` rewrites them, for a deliberate change only.

use newt_core::{assemble_system_prompt, ExposureSettings, PersonaPart, PromptParts, ReadmePart};

const SKILLS: &str = "Available skills (call `use_skill` to load one):\n  fixture-skill: A fixture skill\nTo install a new skill from a local path, run via run_command: `newt skills install <path>` (add `--link` to symlink instead of copy).\n";

fn golden(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/system_prompt")
        .join(format!("{name}.golden"))
}

fn check(name: &str, actual: &str) {
    if std::env::var("NEWT_GOLDEN_UPDATE").as_deref() == Ok("1") {
        std::fs::write(golden(name), actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(golden(name)).expect("golden present");
    assert_eq!(expected, actual, "system prompt golden `{name}` drifted");
}

#[derive(Default)]
struct Variant {
    exposure: ExposureSettings,
    coach: bool,
    persona: Option<PersonaPart<'static>>,
    narration: Option<&'static str>,
    components: newt_core::prompt_components::ComponentSet,
}

fn assembled(v: &Variant) -> String {
    let identity = v.exposure.render_soul(&v.components, v.coach);
    let files = ["Cargo.toml", ".git", "src", "README.md"].map(String::from);
    assemble_system_prompt(&PromptParts {
        components: &v.components,
        identity: &identity,
        coach_identity: v.coach,
        context: Some("## AGENTS.md\nUse four spaces."),
        workspace: "/ws",
        persona: v.persona,
        plan_path: "plan.md",
        publish_guidance: newt_core::agentic::REFACTOR_PUBLISH_GUIDANCE,
        narration_line: v.narration,
        skills_index: Some(SKILLS),
        file_names: Some(&files),
        readme: Some(ReadmePart {
            file_name: "README.md",
            text: "# Fixture\nA tiny project.\n",
        }),
    })
    .text
}

const PERSONA: PersonaPart<'static> = PersonaPart {
    name: "reviewer",
    prompt: "Persona body.",
};

#[test]
fn doer() {
    check("doer", &assembled(&Variant::default()));
}

#[test]
fn coach() {
    let v = Variant {
        coach: true,
        persona: Some(PersonaPart {
            name: "coach",
            ..PERSONA
        }),
        ..Variant::default()
    };
    check("coach", &assembled(&v));
}

#[test]
fn persona() {
    let v = Variant {
        persona: Some(PERSONA),
        ..Variant::default()
    };
    check("persona", &assembled(&v));
}

#[test]
fn read_only() {
    let v = Variant {
        exposure: ExposureSettings {
            read_only: true,
            ..Default::default()
        },
        ..Variant::default()
    };
    check("read_only", &assembled(&v));
}

#[test]
fn allow_list_four() {
    let v = Variant {
        exposure: ExposureSettings {
            allow_list: Some(
                ["read_file", "write_file", "edit_file", "run_command"]
                    .map(String::from)
                    .to_vec(),
            ),
            ..Default::default()
        },
        ..Variant::default()
    };
    check("allow_list_four", &assembled(&v));
}

#[test]
fn narration() {
    let v = Variant {
        narration: Some("**Say what you are doing.** Before each tool call, state in one short sentence what you are about to do and why; then make the call in the same reply."),
        ..Variant::default()
    };
    check("narration", &assembled(&v));
}

/// #2933 PR6: preset `pi` over the pi tool list: one identity line, the tools line,
/// the project instructions, the workspace header and the persona; nothing else.
#[test]
fn pi() {
    let v = Variant {
        exposure: ExposureSettings {
            allow_list: Some(
                newt_core::PI_PRESET_TOOLS
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            ),
            ..Default::default()
        },
        persona: Some(PERSONA),
        components: newt_core::prompt_components::ComponentSet::for_preset(
            newt_core::prompt_components::Preset::Pi,
        ),
        ..Variant::default()
    };
    check("pi", &assembled(&v));
}

/// Behaviour the goldens do not show: the listing is capped and sorted, the
/// README is cut with a marker, and an unreadable directory adds no section.
#[test]
fn listing_and_readme_are_bounded() {
    let names: Vec<String> = (0..50).map(|i| format!("f{i:02}")).collect();
    let long = "x".repeat(3001);
    let base = PromptParts {
        components: &newt_core::prompt_components::ComponentSet::default(),
        identity: "id",
        coach_identity: false,
        context: None,
        workspace: "/w",
        persona: None,
        plan_path: "p",
        publish_guidance: "g",
        narration_line: None,
        skills_index: None,
        file_names: Some(&names),
        readme: Some(ReadmePart {
            file_name: "readme.md",
            text: &long,
        }),
    };
    let text = assemble_system_prompt(&base).text;
    assert!(text.contains("  f39\n") && !text.contains("  f40\n"));
    assert!(text.ends_with(&format!("{}\n...[truncated]\n", "x".repeat(3000))));
    let none = assemble_system_prompt(&PromptParts {
        components: &newt_core::prompt_components::ComponentSet::default(),
        file_names: None,
        readme: None,
        ..base
    })
    .text;
    assert!(!none.contains("Files:") && !none.contains("readme.md"));
}

/// The report of an assembled prompt has no unlabelled text: every byte is a
/// named piece or the join between two of them.
#[test]
fn every_byte_is_labelled() {
    use newt_core::agentic::request_report::RequestReport;
    for v in [
        Variant::default(),
        Variant {
            coach: true,
            persona: Some(PERSONA),
            ..Variant::default()
        },
        Variant {
            narration: Some("Say it."),
            ..Variant::default()
        },
    ] {
        let identity = v.exposure.render_soul(&v.components, v.coach);
        let files = ["a".to_string()];
        let built = assemble_system_prompt(&PromptParts {
            components: &newt_core::prompt_components::ComponentSet::default(),
            identity: &identity,
            coach_identity: v.coach,
            context: Some("## AGENTS.md\nUse four spaces."),
            workspace: "/ws",
            persona: v.persona,
            plan_path: "plan.md",
            publish_guidance: newt_core::agentic::REFACTOR_PUBLISH_GUIDANCE,
            narration_line: v.narration,
            skills_index: Some(SKILLS),
            file_names: Some(&files),
            readme: Some(ReadmePart {
                file_name: "README.md",
                text: "# R\n",
            }),
        });
        // The caller's providers label the context blocks; stand in for them.
        let mut notes = built.notes.clone();
        notes.push(newt_core::agentic::request_report::Annotation {
            label: "project-instructions".into(),
            source: "provider".into(),
            text: "## AGENTS.md\nUse four spaces.".into(),
        });
        let report = RequestReport::build_from_parts(&[built.text.as_str()], None, &notes);
        let stray: Vec<_> = report.system[0]
            .components
            .iter()
            .filter(|c| c.label == "(unattributed)")
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
    }
}
