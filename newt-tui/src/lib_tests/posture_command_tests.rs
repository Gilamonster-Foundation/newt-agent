use super::*;
use newt_core::CaveatsExt as _;
use std::fs;

fn write_skill(root: &std::path::Path, name: &str, body: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: triage guidance\n---\n{body}\n"),
    )
    .unwrap();
}

/// A config wiring `[modes.triage]` → skill + preset, and a matching
/// `[permission_presets.readonly-triage]`. The skills dir is the temp dir.
fn triage_config(skills_dir: &std::path::Path) -> newt_core::Config {
    let mut cfg = newt_core::Config {
        skills: Some(newt_core::SkillsConfig {
            search: vec![skills_dir.to_string_lossy().into_owned()],
            bundled_dir: String::new(),
        }),
        ..newt_core::Config::default()
    };
    cfg.permission_presets.insert(
        "readonly-triage".to_string(),
        newt_core::NamedPermissionPreset {
            // fs_read: None preserves pre-#755 behavior (reads unrestricted).
            fs_read: None,
            readonly: true,
            exec_allow: vec!["git".to_string()],
            deny: vec!["*".to_string()],
            max_calls: Some(40),
        },
    );
    cfg.modes.insert(
        "triage".to_string(),
        newt_core::config::ModeConfig {
            skill: Some("oncall-triage".to_string()),
            preset: Some("readonly-triage".to_string()),
            framing: Some("On-call triage: investigate, do not change prod.".to_string()),
        },
    );
    cfg
}

#[test]
fn posture_status_lists_active_and_available_names() {
    let mut cfg = newt_core::Config::default();
    cfg.modes.insert(
        "triage".to_string(),
        newt_core::config::ModeConfig {
            skill: None,
            preset: Some("readonly-triage".to_string()),
            framing: None,
        },
    );
    cfg.modes.insert(
        "coach".to_string(),
        newt_core::config::ModeConfig {
            skill: None,
            preset: None,
            framing: Some("Ask before acting.".to_string()),
        },
    );
    let active = ActivePosture {
        name: "triage".to_string(),
        preset_name: "readonly-triage".to_string(),
        clamp: newt_core::Caveats::top(),
        clamp_summary: "read-only".to_string(),
        skill_body: None,
        framing: None,
    };

    assert_eq!(
        posture_status_lines(&cfg, Some(&active), true),
        vec![
            "active permission posture: triage — preset 'readonly-triage' floor: read-only",
            "available permission postures: coach, triage",
        ]
    );
    assert_eq!(
        posture_status_lines(&cfg, Some(&active), false),
        vec!["active permission posture: triage — preset 'readonly-triage' floor: read-only"]
    );
}

#[test]
fn posture_status_reports_an_empty_configuration() {
    let cfg = newt_core::Config::default();
    assert_eq!(
            posture_status_lines(&cfg, None, true),
            vec![
                "no active permission posture",
                "available permission postures: (none configured — define [modes.<name>] in your newt config)",
            ]
        );
}

#[test]
fn posture_without_preset_carries_guidance_without_changing_authority() {
    let mut cfg = newt_core::Config::default();
    cfg.modes.insert(
        "coach".to_string(),
        newt_core::config::ModeConfig {
            skill: None,
            preset: None,
            framing: Some("Ask before acting.".to_string()),
        },
    );
    let posture = build_posture("coach", &cfg, |_| panic!("no skill should be loaded")).unwrap();
    let base = newt_core::Caveats::top();

    assert!(posture.permission_clamp().is_none());
    assert_eq!(effective_caveats(&base, Some(&posture)), base);
    assert_eq!(posture.framing.as_deref(), Some("Ask before acting."));
    assert!(
        posture_prompt(&posture).contains("no configured permission floor"),
        "the model must not be told that a guidance-only posture clamps authority"
    );
}

/// The acceptance criterion: `/posture <name>` loads the skill body AND
/// applies the preset clamp in ONE invocation. Uses the real `use_skill`
/// loader (`load_body_from`) over a mock skills dir — no reimplementation.
#[serial_test::serial(real_fs)]
#[test]
fn build_posture_loads_skill_body_and_applies_preset_atomically() {
    // `skill_search_dirs()` appends the HOME-relative `~/.newt/skills`, so
    // hold the env read guard: the cw-400 test (this binary) swaps HOME
    // under a write guard, and a mid-test swap would change what
    // `load_body_from` resolves. Serializes against the writer only.
    let _env = crate::test_env_guard::env_read_guard();
    let skills = tempfile::TempDir::new().unwrap();
    write_skill(skills.path(), "oncall-triage", "Read logs. Do not deploy.");
    let cfg = triage_config(skills.path());
    let dirs = cfg.skill_search_dirs();

    let posture = build_posture("triage", &cfg, |name| {
        newt_skills::load_body_from(&dirs, name)
    })
    .expect("the posture resolves");

    // (a) the skill body was preloaded (same payload as use_skill).
    let body = posture.skill_body.as_deref().expect("skill body");
    assert!(body.contains("Read logs. Do not deploy."), "got: {body}");
    // (b) the preset clamp is applied as a floor.
    assert_eq!(posture.preset_name, "readonly-triage");
    assert!(!posture.clamp.permits_fs_write("/anything"), "readonly");
    assert!(posture.clamp.permits_exec("git"), "allow-listed exec");
    assert!(!posture.clamp.permits_exec("rm"), "deny everything else");
    assert!(!posture.clamp.permits_net("evil.example.com"), "deny=*");
    // (c) the framing is carried for system-prompt injection.
    assert_eq!(
        posture.framing.as_deref(),
        Some("On-call triage: investigate, do not change prod.")
    );
}

/// Atomic-or-nothing: a posture naming a missing preset is an ERROR — never a
/// silent skill-load without the clamp (that would be a false claim).
#[serial_test::serial(real_fs)]
#[test]
fn build_posture_errors_when_the_preset_is_missing() {
    let _env = crate::test_env_guard::env_read_guard(); // HOME-stable: see sibling above
    let skills = tempfile::TempDir::new().unwrap();
    write_skill(skills.path(), "oncall-triage", "body");
    let mut cfg = triage_config(skills.path());
    cfg.permission_presets.clear(); // preset gone, mode still references it
    let dirs = cfg.skill_search_dirs();
    let err = build_posture("triage", &cfg, |name| {
        newt_skills::load_body_from(&dirs, name)
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("readonly-triage"),
        "names the missing preset: {err}"
    );
}

/// A posture naming a missing skill is an ERROR for the same reason — the
/// clamp must not apply without its promised guidance.
#[serial_test::serial(real_fs)]
#[test]
fn build_posture_errors_when_the_skill_is_missing() {
    let _env = crate::test_env_guard::env_read_guard(); // HOME-stable: see sibling above
    let skills = tempfile::TempDir::new().unwrap(); // empty — no skill
    let cfg = triage_config(skills.path());
    let dirs = cfg.skill_search_dirs();
    let err = build_posture("triage", &cfg, |name| {
        newt_skills::load_body_from(&dirs, name)
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("oncall-triage"),
        "names the missing skill: {err}"
    );
}

/// An unknown posture name is an error (no compatibility `[modes.<name>]`).
#[test]
fn build_posture_errors_on_unknown_posture() {
    let cfg = newt_core::Config::default();
    let err = build_posture("nope", &cfg, |_| Ok(String::new()))
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown posture"), "got: {err}");
}

/// The applied posture's effective caveats are base ∩ clamp — strictly
/// attenuated, the floor property at the wiring level.
#[test]
fn effective_caveats_intersect_base_with_the_posture_clamp() {
    let clamp = newt_core::NamedPermissionPreset {
        readonly: true,
        ..Default::default()
    }
    .clamp();
    let posture = ActivePosture {
        name: "triage".to_string(),
        preset_name: "readonly-triage".to_string(),
        clamp_summary: "readonly".to_string(),
        clamp,
        skill_body: None,
        framing: None,
    };
    let base = newt_core::Caveats::top();
    let eff = effective_caveats(&base, Some(&posture));
    assert!(eff.leq(&base), "the posture can only attenuate");
    assert!(!eff.permits_fs_write("/x"), "readonly clamp applied");
    // No posture ⇒ base unchanged (bit-for-bit).
    assert_eq!(effective_caveats(&base, None), base);
}
