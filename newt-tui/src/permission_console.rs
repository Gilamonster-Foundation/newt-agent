//! Human-only permission dashboard and profile editor.
//!
//! This module is deliberately not part of the model tool catalog. It is
//! reachable only from the operator-input `/permissions manage` interceptor.
//! The UI is a plain-scroller-safe modal form: Escape is always Back, while
//! Ctrl-D is the process-wide emergency brake.

use crate::danger::DangerTier;
use crate::permissions::{terminal_safe_quote, terminal_safe_text, PermissionPromptState};
use crate::SessionCapability;
use newt_core::ocap_store::{PolicyFile, Verdict};
use newt_core::{
    CaveatProfile, CaveatsExt as _, DenialKind, PermissionAuthority, PermissionPreset,
    PermissionProfile, PermissionProfileRule, PermissionProfileVerdict, ScopeKeyword, ScopeSpec,
    ToolPermissions, PERMISSION_PROFILE_SCHEMA_VERSION,
};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    Line(String),
    Back,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormValue<T> {
    Value(T),
    Back,
}

trait Console {
    fn ask(&mut self, prompt: &str) -> io::Result<Answer>;
    fn say(&mut self, line: &str);
}

struct StdinConsole;

impl Console for StdinConsole {
    fn ask(&mut self, prompt: &str) -> io::Result<Answer> {
        match crate::modal_input::read_modal_line(prompt)? {
            crate::modal_input::ModalLine::Line(line) => Ok(Answer::Line(line)),
            crate::modal_input::ModalLine::Back => Ok(Answer::Back),
            crate::modal_input::ModalLine::Eof => Ok(Answer::Eof),
        }
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

pub(crate) struct PermissionConsoleContext<'a> {
    pub(crate) state: &'a mut PermissionPromptState,
    pub(crate) prompt_enabled: &'a mut bool,
    pub(crate) capability: &'a mut SessionCapability,
    pub(crate) workspace: &'a str,
    pub(crate) config_path: Option<&'a Path>,
    pub(crate) key_path: Option<&'a Path>,
    pub(crate) log_path: Option<&'a Path>,
    pub(crate) conversation_id: &'a str,
    /// Profiles applied during this process, in first-application order. Their
    /// authority effects are cumulative even when a later profile is broader.
    pub(crate) active_profiles: &'a mut Vec<String>,
    /// The cumulative meet-only ceiling of every profile applied this session.
    /// This is kept separately from `SessionCapability`: prompted grants
    /// intentionally re-mint from the operator root, so the gate must re-apply
    /// every prior profile clamp after an allow-once/session widening.
    pub(crate) active_profile_clamp: &'a mut Option<newt_core::Caveats>,
}

pub(crate) fn run_manage(ctx: &mut PermissionConsoleContext<'_>) -> anyhow::Result<()> {
    let mut console = StdinConsole;
    run_manage_with(&mut console, ctx)
}

fn run_manage_with(
    console: &mut dyn Console,
    ctx: &mut PermissionConsoleContext<'_>,
) -> anyhow::Result<()> {
    loop {
        console.say("");
        console.say("Permission console — operator only");
        console.say("  1  Review effective authority, policy, and decisions");
        console.say("  2  Enable/disable ordinary permission prompts");
        console.say("  3  Approve an exact target for this session");
        console.say("  4  Disable an exact target for this session");
        console.say("  5  Permanently approve an exact low-danger target");
        console.say("  6  Permanently deny an exact target");
        console.say("  7  Permission profiles (list/apply/make/edit/import/export)");
        console.say("  Esc  Back to chat · Ctrl+D  Emergency brake — STOP RIGHT NOW");
        match console.ask("permission> ")? {
            Answer::Back | Answer::Eof => return Ok(()),
            Answer::Line(choice) => match choice.trim().to_ascii_lowercase().as_str() {
                "1" | "review" => render_review(console, ctx),
                "2" | "prompt" | "prompts" => configure_prompting(console, ctx)?,
                "3" | "approve" | "session approve" => configure_session_rule(console, ctx, true)?,
                "4" | "disable" | "deny" | "session deny" => {
                    configure_session_rule(console, ctx, false)?;
                }
                "5" | "permanent approve" => {
                    configure_durable_rule(console, ctx, Verdict::Approve)?;
                }
                "6" | "permanent deny" => configure_durable_rule(console, ctx, Verdict::Deny)?,
                "7" | "profile" | "profiles" => profiles_menu(console, ctx)?,
                "" | "b" | "back" | "q" => return Ok(()),
                _ => console.say("Unknown choice. Escape always goes Back."),
            },
        }
    }
}

fn render_review(console: &mut dyn Console, ctx: &PermissionConsoleContext<'_>) {
    console.say("Effective authority (available, not proof of use):");
    for line in caveat_lines(ctx.capability.caveats()) {
        console.say(&format!("  {line}"));
    }
    console.say(&format!(
        "Ordinary denial prompts: {}",
        if *ctx.prompt_enabled { "ON" } else { "OFF" }
    ));
    let profiles = if ctx.active_profiles.is_empty() {
        "none".to_string()
    } else {
        ctx.active_profiles
            .iter()
            .map(|name| terminal_safe_quote(name))
            .collect::<Vec<_>>()
            .join(" -> ")
    };
    console.say(&format!(
        "Permission profiles applied (cumulative): {profiles}"
    ));
    let snapshot = ctx.state.operator_snapshot();
    render_pairs(console, "Session approvals", &snapshot.session_grants);
    render_pairs(console, "Session disables", &snapshot.session_denials);
    render_pairs(
        console,
        "Legacy permanent disables",
        &snapshot.persistent_denials,
    );
    render_pairs(console, "Profile ask rules", &snapshot.profile_asks);
    render_pairs(console, "Profile deny rules", &snapshot.profile_denials);

    console.say("Durable OCAP rules:");
    let mut durable_count = 0;
    for verdict in newt_core::ocap_store::VERDICTS {
        if let Some(file) = snapshot.ocap_policy.files.get(&verdict) {
            durable_count += render_policy_file(console, verdict, file);
        }
    }
    if durable_count == 0 {
        console.say("  (none)");
    }

    console.say("Prompt decisions this session:");
    render_prompt_decisions(console, &snapshot.decisions);

    console.say("Ambient authority preflights this session:");
    if snapshot.authority_events.is_empty() {
        console.say("  (none)");
    } else {
        for event in &snapshot.authority_events {
            console.say(&format!(
                "  {} {}:{} via {} — {} ({})",
                terminal_safe_text(&event.ts_claim),
                event.kind.as_str(),
                terminal_safe_quote(&event.target),
                terminal_safe_quote(&event.tool),
                terminal_safe_text(&event.outcome),
                terminal_safe_text(&event.basis)
            ));
        }
    }

    let journal_path =
        std::env::var_os(newt_core::denial_journal::DENIAL_JOURNAL_PATH_ENV).map(PathBuf::from);
    match journal_path
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok())
    {
        Some(body) => {
            let summaries =
                newt_core::denial_journal::summarize(&newt_core::denial_journal::read_jsonl(&body));
            console.say("Confinement denials (repair evidence, not authority):");
            if summaries.is_empty() {
                console.say("  (none)");
            } else {
                for item in summaries {
                    console.say(&format!(
                        "  {}:{} ×{} [{}] — {}",
                        terminal_safe_text(&item.kind),
                        terminal_safe_quote(&item.target),
                        item.count,
                        item.classification.as_str(),
                        terminal_safe_quote(&item.example_command)
                    ));
                }
            }
        }
        None => console
            .say("Confinement denials: no readable journal (the decision log is not a usage log)."),
    }
}

fn render_prompt_decisions(console: &mut dyn Console, decisions: &[newt_core::PermissionRecord]) {
    if decisions.is_empty() {
        console.say("  (none)");
    } else {
        for decision in decisions {
            console.say(&format!(
                "  {} {} {}:{} via {}",
                terminal_safe_text(&decision.decision),
                terminal_safe_text(&decision.scope),
                terminal_safe_text(&decision.kind),
                terminal_safe_quote(&decision.target),
                terminal_safe_quote(&decision.tool)
            ));
            if let Some(context) = decision.context.as_deref() {
                console.say("    review context (secret-redacted, bounded; untrusted):");
                for line in context.split('\n') {
                    console.say(&format!("      {}", terminal_safe_text(line)));
                }
            }
        }
    }
}

fn render_pairs(console: &mut dyn Console, label: &str, pairs: &[(DenialKind, String)]) {
    console.say(&format!("{label}:"));
    if pairs.is_empty() {
        console.say("  (none)");
    } else {
        for (kind, target) in pairs {
            console.say(&format!(
                "  {}:{}",
                kind.as_str(),
                terminal_safe_quote(target)
            ));
        }
    }
}

fn render_policy_file(console: &mut dyn Console, verdict: Verdict, file: &PolicyFile) -> usize {
    let mut count = 0;
    for entry in &file.exec {
        console.say(&format!(
            "  {verdict:?} exec:{}",
            terminal_safe_quote(&entry.target)
        ));
        count += 1;
    }
    for entry in &file.fs {
        console.say(&format!(
            "  {verdict:?} {}:{}",
            if entry.write { "fs_write" } else { "fs_read" },
            terminal_safe_quote(&entry.path)
        ));
        count += 1;
    }
    for entry in &file.net {
        console.say(&format!(
            "  {verdict:?} net:{}",
            terminal_safe_quote(&entry.host)
        ));
        count += 1;
    }
    count
}

fn caveat_lines(caveats: &newt_core::Caveats) -> Vec<String> {
    vec![
        format!("fs_read  = {}", scope_text(&caveats.fs_read)),
        format!("fs_write = {}", scope_text(&caveats.fs_write)),
        format!("exec     = {}", scope_text(&caveats.exec)),
        format!("net      = {}", scope_text(&caveats.net)),
        format!("max_calls = {:?}", caveats.max_calls),
    ]
}

fn scope_text(scope: &newt_core::Scope<String>) -> String {
    match scope {
        newt_core::Scope::All => "all".to_string(),
        newt_core::Scope::Only(items) if items.is_empty() => "none".to_string(),
        newt_core::Scope::Only(items) => {
            let mut items: Vec<_> = items.iter().map(|item| terminal_safe_quote(item)).collect();
            items.sort();
            format!("[{}]", items.join(", "))
        }
    }
}

fn configure_prompting(
    console: &mut dyn Console,
    ctx: &mut PermissionConsoleContext<'_>,
) -> anyhow::Result<()> {
    console.say(&format!(
        "Ordinary permission prompting is currently {}.",
        if *ctx.prompt_enabled { "ON" } else { "OFF" }
    ));
    console
        .say("Explicit ask/passkey policy can still require review when ordinary prompts are off.");
    match console.ask("Set [on/off] (Esc=Back): ")? {
        Answer::Line(value) if matches!(value.trim(), "on" | "ON" | "enable" | "1") => {
            *ctx.prompt_enabled = true;
            console.say("Ordinary permission prompting enabled for this session.");
        }
        Answer::Line(value) if matches!(value.trim(), "off" | "OFF" | "disable" | "0") => {
            *ctx.prompt_enabled = false;
            console.say("Ordinary permission prompting disabled for this session.");
        }
        Answer::Line(_) => console.say("No change: expected `on` or `off`."),
        Answer::Back | Answer::Eof => {}
    }
    Ok(())
}

fn session_approval_blocker(
    snapshot: &crate::permissions::OperatorPermissionSnapshot,
    kind: DenialKind,
    target: &str,
) -> Option<&'static str> {
    let key = (kind, target.to_string());
    if snapshot.persistent_denials.contains(&key) {
        return Some("legacy permanent disable");
    }
    if snapshot.profile_denials.contains(&key) {
        return Some("active profile deny");
    }
    if snapshot.profile_asks.contains(&key) {
        return Some("active profile ask (must be decided at the operation)");
    }
    // A durable Ask is intentionally overridable by an explicit local session
    // approval. Passkey is not: it is a step-up requirement, and the gate
    // checks it before session grants.
    match newt_core::ocap_store::evaluate_request(&snapshot.ocap_policy, kind, target) {
        Some(Verdict::Deny) => Some("durable deny"),
        Some(Verdict::Passkey) => Some("durable passkey/step-up requirement"),
        Some(Verdict::Ask | Verdict::Approve) | None => None,
    }
}

fn profile_ceiling_permits(
    ceiling: &newt_core::Caveats,
    kind: DenialKind,
    target: &str,
    workspace: &str,
) -> bool {
    let path_permitted = |scope: &newt_core::Scope<String>| match scope {
        newt_core::Scope::All => true,
        newt_core::Scope::Only(roots) => roots.iter().any(|root| {
            let root = crate::permissions::normalize_permission_target(
                kind,
                root,
                Some(Path::new(workspace)),
            );
            Path::new(target).starts_with(Path::new(&root))
        }),
    };

    match kind {
        DenialKind::Exec => ceiling.permits_exec(target),
        DenialKind::FsRead => path_permitted(&ceiling.fs_read),
        DenialKind::FsWrite => path_permitted(&ceiling.fs_write),
        DenialKind::Net => ceiling.permits_net(target),
        DenialKind::GitWrite => {
            newt_core::git_caveats::GitCaveats::from_session(ceiling).permits_commit()
        }
        DenialKind::RemoteTool | DenialKind::ShellConstruct => true,
    }
}

fn profile_ceiling_masks(
    ctx: &PermissionConsoleContext<'_>,
    kind: DenialKind,
    target: &str,
) -> bool {
    ctx.active_profile_clamp
        .as_ref()
        .is_some_and(|ceiling| !profile_ceiling_permits(ceiling, kind, target, ctx.workspace))
}

fn configure_session_rule(
    console: &mut dyn Console,
    ctx: &mut PermissionConsoleContext<'_>,
    approve: bool,
) -> anyhow::Result<()> {
    let Some(kind) = ask_kind(console)? else {
        return Ok(());
    };
    let Some(target) = ask_nonempty(console, "Exact target (Esc=Back): ")? else {
        return Ok(());
    };
    let target = crate::permissions::normalize_permission_target(
        kind,
        &target,
        Some(Path::new(ctx.workspace)),
    );
    if approve {
        let snapshot = ctx.state.operator_snapshot();
        if let Some(source) = session_approval_blocker(&snapshot, kind, &target) {
            console.say(&format!(
                "Not approved: {source} still governs {}:{}. Remove or change that \
                 narrowing rule first.",
                kind.as_str(),
                terminal_safe_quote(&target)
            ));
            return Ok(());
        }
        if profile_ceiling_masks(ctx, kind, &target) {
            console.say(&format!(
                "Not approved: the cumulative permission-profile caveat ceiling still denies \
                 {}:{}. A session approval would be ineffective; restart and apply only profiles \
                 whose cumulative ceiling permits it.",
                kind.as_str(),
                terminal_safe_quote(&target)
            ));
            return Ok(());
        }
    }
    let danger = crate::permissions::production_danger_table();
    if approve && danger.classify(kind, &target) == DangerTier::High {
        console
            .say("Refused: high-danger targets are allow-once only; no standing session approval.");
        return Ok(());
    }
    if approve {
        ctx.state.session_approve(kind, &target);
    } else {
        ctx.state.session_deny(kind, &target);
    }
    record_console_decision(
        ctx,
        kind,
        &target,
        if approve { "allow" } else { "deny" },
        "session",
    );
    console.say(&format!(
        "{} {}:{} for this session.",
        if approve { "Approved" } else { "Disabled" },
        kind.as_str(),
        terminal_safe_quote(&target)
    ));
    Ok(())
}

fn configure_durable_rule(
    console: &mut dyn Console,
    ctx: &mut PermissionConsoleContext<'_>,
    verdict: Verdict,
) -> anyhow::Result<()> {
    let Some(config_path) = ctx.config_path else {
        console.say("No user config path is available; durable policy was not changed.");
        return Ok(());
    };
    let Some(kind) = ask_kind(console)? else {
        return Ok(());
    };
    if newt_core::ocap_store::class_for(kind).is_none() {
        console.say("That authority has no durable OCAP class; use a session decision.");
        return Ok(());
    }
    let Some(target) = ask_nonempty(console, "Exact target (Esc=Back): ")? else {
        return Ok(());
    };
    let target = crate::permissions::normalize_permission_target(
        kind,
        &target,
        Some(Path::new(ctx.workspace)),
    );
    let profile_masked = verdict == Verdict::Approve && profile_ceiling_masks(ctx, kind, &target);
    if verdict == Verdict::Approve {
        let snapshot = ctx.state.operator_snapshot();
        let key = (kind, target.clone());
        let blocking_source = if snapshot.persistent_denials.contains(&key) {
            Some("legacy permanent disable")
        } else if snapshot.profile_denials.contains(&key) {
            Some("active profile deny")
        } else if snapshot.profile_asks.contains(&key) {
            Some("active profile ask")
        } else {
            None
        };
        if let Some(source) = blocking_source {
            console.say(&format!(
                "Refused: {source} still governs {}:{}; a durable approval would not \
                 enable it. Remove or change that narrowing rule first.",
                kind.as_str(),
                terminal_safe_quote(&target)
            ));
            return Ok(());
        }
    }
    if profile_masked {
        console.say(
            "This durable approval can be stored for a future session, but the cumulative \
             permission-profile caveat ceiling masks it in the current process.",
        );
    }
    let note = match console.ask("Note (optional, Esc=Back): ")? {
        Answer::Line(note) => (!note.trim().is_empty()).then_some(note),
        Answer::Back | Answer::Eof => return Ok(()),
    };
    console.say(&format!(
        "Will write {:?} {}:{} to {}.",
        verdict,
        kind.as_str(),
        terminal_safe_quote(&target),
        terminal_safe_quote(&config_path.with_file_name("ocap").to_string_lossy())
    ));
    let confirmed = match console.ask("Confirm [y/N] (Esc=Back): ")? {
        Answer::Line(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Answer::Back | Answer::Eof => false,
    };
    if !confirmed {
        console.say("No durable change.");
        return Ok(());
    }

    let user = match ctx.key_path {
        Some(key_path) => Some(newt_identity::load_or_generate(key_path)?),
        None if verdict == Verdict::Approve => {
            anyhow::bail!("no local operator signing-key path is available")
        }
        None => None,
    };
    let signer = |payload: &[u8]| {
        user.as_ref()
            .expect("approve signer is present")
            .sign(payload)
            .to_bytes()
    };
    let signer_ref: Option<newt_core::ocap_store::PolicyEntrySigner<'_>> =
        (verdict == Verdict::Approve).then_some(&signer);
    let granted = chrono::Utc::now().to_rfc3339();
    newt_core::ocap_store::update_policy_target(
        config_path,
        newt_core::ocap_store::PolicyTargetUpdate {
            verdict,
            kind,
            target: &target,
            note: note.as_deref(),
            granted: Some(&granted),
            by: Some("human:permission-console"),
        },
        crate::ocap_high_danger_predicate(),
        signer_ref,
    )
    .map_err(anyhow::Error::msg)?;
    let effective = reload_ocap_policy(
        ctx,
        config_path,
        user.as_ref().map(|key| key.public().as_bytes()),
        kind,
        &target,
        console,
    );
    if effective != Some(verdict) {
        anyhow::bail!(
            "durable policy was written, but the verified live verdict for {}:{} is \
             {:?}, not {:?}; no success was recorded",
            kind.as_str(),
            terminal_safe_quote(&target),
            effective,
            verdict
        );
    }
    if verdict == Verdict::Approve && !profile_masked {
        // The durable transition replaced any durable deny, and the checks
        // above already refused legacy/profile denies. Clear a matching
        // session disable too so the console never reports an approval that
        // its own live overlay immediately vetoes.
        ctx.state.session_approve(kind, &target);
    } else {
        ctx.state.session_deny(kind, &target);
    }
    record_console_decision(
        ctx,
        kind,
        &target,
        if verdict == Verdict::Approve {
            "allow"
        } else {
            "deny"
        },
        "permanent",
    );
    if profile_masked {
        console.say(&format!(
            "Durable {:?} verified for {}:{}, but it is currently masked by the cumulative \
             permission-profile caveat ceiling; it can take effect only in a future process \
             whose applied profiles permit it.",
            verdict,
            kind.as_str(),
            terminal_safe_quote(&target)
        ));
    } else {
        console.say(&format!(
            "Durable {:?} verified for {}:{}; the matching live session overlay now agrees.",
            verdict,
            kind.as_str(),
            terminal_safe_quote(&target)
        ));
    }
    Ok(())
}

fn reload_ocap_policy(
    ctx: &mut PermissionConsoleContext<'_>,
    config_path: &Path,
    verifying_key: Option<[u8; 32]>,
    kind: DenialKind,
    target: &str,
    console: &mut dyn Console,
) -> Option<Verdict> {
    let (policy, warnings) = newt_core::ocap_store::load_store(config_path, verifying_key);
    for warning in warnings {
        console.say(&format!("Policy warning: {}", terminal_safe_text(&warning)));
    }
    // Compute the exact verdict before moving the verified store into the live
    // gate. Callers use this to avoid false success after a failed signature
    // check or a stricter competing verdict.
    let effective = newt_core::ocap_store::evaluate_request(&policy, kind, target);
    ctx.state.replace_ocap_policy(policy);
    effective
}

fn record_console_decision(
    ctx: &mut PermissionConsoleContext<'_>,
    kind: DenialKind,
    target: &str,
    decision: &str,
    scope: &str,
) {
    let record = newt_core::PermissionRecord::new(
        ctx.conversation_id,
        "permission_console",
        kind,
        target,
        decision,
        scope,
    );
    if let Some(path) = ctx.log_path {
        let _ = record.append_jsonl(path);
    }
    ctx.state.push_decision(record);
}

fn profiles_dir(config_path: Option<&Path>) -> Option<PathBuf> {
    config_path.map(|path| path.with_file_name("permission-profiles"))
}

fn profiles_menu(
    console: &mut dyn Console,
    ctx: &mut PermissionConsoleContext<'_>,
) -> anyhow::Result<()> {
    loop {
        console.say("");
        console.say("Permission profiles — portable TOML; imported approval candidates are inert");
        console.say("  1 list   2 apply   3 make/edit   4 import   5 export   Esc Back");
        match console.ask("profiles> ")? {
            Answer::Back | Answer::Eof => return Ok(()),
            Answer::Line(choice) => match choice.trim().to_ascii_lowercase().as_str() {
                "1" | "list" => list_profiles(console, ctx),
                "2" | "apply" => apply_profile_form(console, ctx)?,
                "3" | "make" | "edit" | "make/edit" => edit_profile_form(console, ctx)?,
                "4" | "import" => import_profile_form(console, ctx)?,
                "5" | "export" => export_profile_form(console, ctx)?,
                "" | "b" | "back" | "q" => return Ok(()),
                _ => console.say("Unknown profile action. Escape always goes Back."),
            },
        }
    }
}

fn load_profiles(
    ctx: &PermissionConsoleContext<'_>,
) -> (
    std::collections::BTreeMap<String, PermissionProfile>,
    Vec<String>,
) {
    match profiles_dir(ctx.config_path) {
        Some(dir) => newt_core::load_permission_profiles(&dir),
        None => (
            newt_core::builtin_permission_profiles(),
            vec![
                "no user config path is available; built-in profiles are read-only this session"
                    .to_string(),
            ],
        ),
    }
}

fn list_profiles(console: &mut dyn Console, ctx: &PermissionConsoleContext<'_>) {
    let (profiles, warnings) = load_profiles(ctx);
    for warning in warnings {
        console.say(&format!("Warning: {}", terminal_safe_text(&warning)));
    }
    for profile in profiles.values() {
        console.say(&format!(
            "  {:<24} {:<14} prompts={} — {}",
            terminal_safe_quote(&profile.name),
            profile.base_preset.as_str(),
            if profile.prompt { "on" } else { "off" },
            terminal_safe_quote(&profile.description)
        ));
    }
}

/// Applying profiles is monotonic for the process lifetime. The live session
/// capability already meets every requested profile into its current
/// authority; retain the same cumulative ceiling for prompted re-mints, which
/// otherwise re-root from the operator key.
fn accumulate_profile_clamp(active: &mut Option<newt_core::Caveats>, next: newt_core::Caveats) {
    *active = Some(match active.take() {
        Some(current) => current.meet(&next),
        None => next,
    });
}

/// The complete ceiling selected by a profile.
///
/// `base_preset` is authority-bearing policy too: remembering only the
/// profile's explicit `clamp` would let a later operator-root re-mint pierce a
/// restrictive base preset when that clamp is left at its default (`top`).
fn requested_profile_caveats(profile: &PermissionProfile, workspace: &str) -> newt_core::Caveats {
    let base = ToolPermissions {
        preset: profile.base_preset.clone(),
        prompt: profile.prompt,
        ..ToolPermissions::default()
    }
    .to_caveats(workspace);
    base.meet(&profile.clamp.to_caveats())
}

fn apply_profile_form(
    console: &mut dyn Console,
    ctx: &mut PermissionConsoleContext<'_>,
) -> anyhow::Result<()> {
    list_profiles(console, ctx);
    let Some(name) = ask_nonempty(console, "Apply which profile (Esc=Back): ")? else {
        return Ok(());
    };
    let (profiles, warnings) = load_profiles(ctx);
    for warning in warnings {
        console.say(&format!("Warning: {}", terminal_safe_text(&warning)));
    }
    let Some(profile) = profiles.get(&name) else {
        console.say("No such permission profile.");
        return Ok(());
    };
    let requested = requested_profile_caveats(profile, ctx.workspace);
    let clamped = ctx.capability.reapply_policy(requested.clone());
    *ctx.prompt_enabled = profile.prompt;
    let candidates = ctx.state.apply_profile_rules(&profile.rules);
    if !ctx.active_profiles.contains(&profile.name) {
        ctx.active_profiles.push(profile.name.clone());
    }
    accumulate_profile_clamp(ctx.active_profile_clamp, requested);
    console.say(&format!(
        "Applied permission profile {}.",
        terminal_safe_quote(&profile.name)
    ));
    if clamped {
        console.say(
            "The profile requested authority wider than this live session; it was clamped. \
             Restart Newt under the broader base policy to widen.",
        );
    }
    if let Some(persona) = &profile.persona {
        console.say(&format!(
            "Profile suggests persona {}; persona changes remain explicit via /persona.",
            terminal_safe_quote(persona)
        ));
    }
    if !candidates.is_empty() {
        console.say("Approval candidates were NOT granted; review them locally:");
        for (kind, target) in candidates {
            console.say(&format!(
                "  {}:{}",
                kind.as_str(),
                terminal_safe_quote(&target)
            ));
        }
    }
    Ok(())
}

fn edit_profile_form(
    console: &mut dyn Console,
    ctx: &PermissionConsoleContext<'_>,
) -> anyhow::Result<()> {
    let Some(profile_dir) = profiles_dir(ctx.config_path) else {
        console.say("No user config path is available; profile files cannot be edited.");
        return Ok(());
    };
    let (profiles, warnings) = load_profiles(ctx);
    for warning in warnings {
        console.say(&format!("Warning: {}", terminal_safe_text(&warning)));
    }
    let Some(name) = ask_nonempty(console, "Profile name (new or existing, Esc=Back): ")? else {
        return Ok(());
    };
    let mut profile = profiles.get(&name).cloned().unwrap_or(PermissionProfile {
        schema_version: PERMISSION_PROFILE_SCHEMA_VERSION,
        name: name.clone(),
        description: String::new(),
        base_preset: PermissionPreset::WorkspaceDev,
        prompt: true,
        persona: None,
        clamp: CaveatProfile::default(),
        rules: Vec::new(),
    });
    profile.name = name;
    match ask_keep(console, "Description", &profile.description)? {
        FormValue::Value(Some(value)) => profile.description = value,
        FormValue::Value(None) => {}
        FormValue::Back => return Ok(()),
    }
    match ask_keep(
        console,
        "Base preset (read_only/workspace_edit/workspace_dev/full_access)",
        profile.base_preset.as_str(),
    )? {
        FormValue::Value(Some(value)) => {
            profile.base_preset = parse_preset(&value)
                .ok_or_else(|| anyhow::anyhow!("unknown permission preset '{value}'"))?;
        }
        FormValue::Value(None) => {}
        FormValue::Back => return Ok(()),
    }
    match ask_keep(
        console,
        "Prompt on denial (true/false)",
        if profile.prompt { "true" } else { "false" },
    )? {
        FormValue::Value(Some(value)) => {
            profile.prompt = parse_bool(&value)
                .ok_or_else(|| anyhow::anyhow!("expected true/false, got '{value}'"))?;
        }
        FormValue::Value(None) => {}
        FormValue::Back => return Ok(()),
    }
    profile.persona = match ask_optional(console, "Suggested persona", profile.persona.as_deref())?
    {
        FormValue::Value(value) => value,
        FormValue::Back => return Ok(()),
    };
    profile.clamp.fs_read = match ask_scope(console, "Clamp fs_read", &profile.clamp.fs_read)? {
        FormValue::Value(value) => value,
        FormValue::Back => return Ok(()),
    };
    profile.clamp.fs_write = match ask_scope(console, "Clamp fs_write", &profile.clamp.fs_write)? {
        FormValue::Value(value) => value,
        FormValue::Back => return Ok(()),
    };
    profile.clamp.exec = match ask_scope(console, "Clamp exec", &profile.clamp.exec)? {
        FormValue::Value(value) => value,
        FormValue::Back => return Ok(()),
    };
    profile.clamp.net = match ask_scope(console, "Clamp net", &profile.clamp.net)? {
        FormValue::Value(value) => value,
        FormValue::Back => return Ok(()),
    };

    console.say("Existing rules:");
    if profile.rules.is_empty() {
        console.say("  (none)");
    } else {
        for rule in &profile.rules {
            console.say(&format!(
                "  {:?} {:?} {}",
                rule.verdict,
                rule.authority,
                terminal_safe_quote(&rule.target)
            ));
        }
    }
    let replace = match console.ask("Replace rules? [y/N] (Esc=Back): ")? {
        Answer::Line(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Answer::Back | Answer::Eof => return Ok(()),
    };
    if replace {
        profile.rules.clear();
        console
            .say("Enter rules as `deny|ask|approve_candidate authority target`; blank finishes.");
        loop {
            match console.ask("rule> ")? {
                Answer::Back | Answer::Eof => return Ok(()),
                Answer::Line(line) if line.trim().is_empty() => break,
                Answer::Line(line) => match parse_profile_rule(&line) {
                    Ok(rule) => profile.rules.push(rule),
                    Err(error) => {
                        console.say(&format!("Invalid rule: {}", terminal_safe_text(&error)));
                    }
                },
            }
        }
    }

    let preview = profile
        .to_toml()
        .map_err(anyhow::Error::msg)?
        .lines()
        .map(terminal_safe_text)
        .collect::<Vec<_>>()
        .join("\n");
    console.say(&format!(
        "\n# {}\n{preview}",
        terminal_safe_quote(
            &profile_dir
                .join(format!("{}.toml", profile.name))
                .to_string_lossy()
        )
    ));
    let confirmed = match console.ask("Write this profile? [y/N] (Esc=Back): ")? {
        Answer::Line(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Answer::Back | Answer::Eof => false,
    };
    if confirmed {
        let path = newt_core::save_permission_profile(&profile_dir, &profile)
            .map_err(anyhow::Error::msg)?;
        console.say(&format!(
            "Wrote {}.",
            terminal_safe_quote(&path.to_string_lossy())
        ));
    } else {
        console.say("Nothing written.");
    }
    Ok(())
}

fn import_profile_form(
    console: &mut dyn Console,
    ctx: &PermissionConsoleContext<'_>,
) -> anyhow::Result<()> {
    let Some(profile_dir) = profiles_dir(ctx.config_path) else {
        console.say("No user config path is available; profiles cannot be installed.");
        return Ok(());
    };
    let Some(source) = ask_nonempty(console, "Import TOML path (Esc=Back): ")? else {
        return Ok(());
    };
    let profile =
        newt_core::load_permission_profile(Path::new(&source)).map_err(anyhow::Error::msg)?;
    console.say(&format!(
        "Validated {}. Approval candidates remain inert.",
        terminal_safe_quote(&profile.name)
    ));
    let destination = profile_dir.join(format!("{}.toml", profile.name));
    let prompt = if destination.exists() {
        "Replace the existing local profile? [y/N] (Esc=Back): "
    } else {
        "Install locally? [y/N] (Esc=Back): "
    };
    let confirmed = match console.ask(prompt)? {
        Answer::Line(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Answer::Back | Answer::Eof => false,
    };
    if confirmed {
        let path = newt_core::save_permission_profile(&profile_dir, &profile)
            .map_err(anyhow::Error::msg)?;
        console.say(&format!(
            "Installed {}.",
            terminal_safe_quote(&path.to_string_lossy())
        ));
    }
    Ok(())
}

fn export_profile_form(
    console: &mut dyn Console,
    ctx: &PermissionConsoleContext<'_>,
) -> anyhow::Result<()> {
    list_profiles(console, ctx);
    let Some(name) = ask_nonempty(console, "Export which profile (Esc=Back): ")? else {
        return Ok(());
    };
    let (profiles, _) = load_profiles(ctx);
    let profile = profiles
        .get(&name)
        .ok_or_else(|| anyhow::anyhow!("no permission profile named '{name}'"))?;
    let Some(destination) = ask_nonempty(console, "Destination TOML path (Esc=Back): ")? else {
        return Ok(());
    };
    let path = PathBuf::from(destination);
    if path.exists() {
        let replace = match console.ask("Replace existing export? [y/N] (Esc=Back): ")? {
            Answer::Line(value) => {
                matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes")
            }
            Answer::Back | Answer::Eof => false,
        };
        if !replace {
            console.say("Nothing written.");
            return Ok(());
        }
    }
    newt_core::write_permission_profile(&path, profile).map_err(anyhow::Error::msg)?;
    console.say(&format!(
        "Exported shareable profile to {}.",
        terminal_safe_quote(&path.to_string_lossy())
    ));
    Ok(())
}

fn ask_kind(console: &mut dyn Console) -> io::Result<Option<DenialKind>> {
    console
        .say("Authorities: exec, fs_read, fs_write, net, remote_tool, git_write, shell_construct");
    match console.ask("Authority (Esc=Back): ")? {
        Answer::Line(value) => match value.trim().parse::<DenialKind>() {
            Ok(kind) => Ok(Some(kind)),
            Err(()) => {
                console.say("Unknown authority.");
                Ok(None)
            }
        },
        Answer::Back | Answer::Eof => Ok(None),
    }
}

fn ask_nonempty(console: &mut dyn Console, prompt: &str) -> io::Result<Option<String>> {
    match console.ask(prompt)? {
        Answer::Line(value) if value.trim().is_empty() => Ok(None),
        Answer::Line(value) => Ok(Some(value.trim().to_string())),
        Answer::Back | Answer::Eof => Ok(None),
    }
}

fn ask_keep(
    console: &mut dyn Console,
    label: &str,
    current: &str,
) -> io::Result<FormValue<Option<String>>> {
    match console.ask(&format!(
        "{label} [{}] (blank=keep, Esc=Back): ",
        terminal_safe_quote(current)
    ))? {
        Answer::Line(value) if value.is_empty() => Ok(FormValue::Value(None)),
        Answer::Line(value) => Ok(FormValue::Value(Some(value))),
        Answer::Back | Answer::Eof => Ok(FormValue::Back),
    }
}

fn ask_optional(
    console: &mut dyn Console,
    label: &str,
    current: Option<&str>,
) -> io::Result<FormValue<Option<String>>> {
    match console.ask(&format!(
        "{label} [{}] (blank=keep, '-'=none, Esc=Back): ",
        current
            .map(terminal_safe_quote)
            .unwrap_or_else(|| "none".to_string())
    ))? {
        Answer::Line(value) if value.is_empty() => {
            Ok(FormValue::Value(current.map(str::to_string)))
        }
        Answer::Line(value) if value == "-" => Ok(FormValue::Value(None)),
        Answer::Line(value) => Ok(FormValue::Value(Some(value))),
        Answer::Back | Answer::Eof => Ok(FormValue::Back),
    }
}

fn ask_scope(
    console: &mut dyn Console,
    label: &str,
    current: &ScopeSpec,
) -> io::Result<FormValue<ScopeSpec>> {
    match console.ask(&format!(
        "{label} [{}] (all/none/comma-list; blank=keep; Esc=Back): ",
        terminal_safe_quote(&current.summary())
    ))? {
        Answer::Line(value) if value.trim().is_empty() => Ok(FormValue::Value(current.clone())),
        Answer::Line(value) => parse_scope(&value)
            .map(FormValue::Value)
            .map_err(io::Error::other),
        Answer::Back | Answer::Eof => Ok(FormValue::Back),
    }
}

fn parse_scope(value: &str) -> Result<ScopeSpec, String> {
    match value.trim() {
        "all" => Ok(ScopeSpec::Keyword(ScopeKeyword::All)),
        "none" => Ok(ScopeSpec::Keyword(ScopeKeyword::None)),
        "" => Err("scope is empty".to_string()),
        value => {
            let items: Vec<_> = value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect();
            if items.is_empty() {
                Err("scope list is empty".to_string())
            } else {
                Ok(ScopeSpec::Items(items))
            }
        }
    }
}

fn parse_preset(value: &str) -> Option<PermissionPreset> {
    match value.trim() {
        "read_only" | "readonly" => Some(PermissionPreset::ReadOnly),
        "workspace_edit" | "edit" => Some(PermissionPreset::WorkspaceEdit),
        "workspace_dev" | "developer" | "dev" => Some(PermissionPreset::WorkspaceDev),
        "full_access" | "full" => Some(PermissionPreset::FullAccess),
        "custom" => Some(PermissionPreset::Custom),
        _ => None,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn parse_profile_rule(line: &str) -> Result<PermissionProfileRule, String> {
    let mut parts = line.splitn(3, char::is_whitespace);
    let verdict = match parts.next().unwrap_or("") {
        "deny" => PermissionProfileVerdict::Deny,
        "ask" => PermissionProfileVerdict::Ask,
        "approve_candidate" | "approve-candidate" => PermissionProfileVerdict::ApproveCandidate,
        other => return Err(format!("unknown verdict '{other}'")),
    };
    let authority = match parts.next().unwrap_or("") {
        "exec" => PermissionAuthority::Exec,
        "fs_read" => PermissionAuthority::FsRead,
        "fs_write" => PermissionAuthority::FsWrite,
        "net" => PermissionAuthority::Net,
        "remote_tool" => PermissionAuthority::RemoteTool,
        "git_write" => PermissionAuthority::GitWrite,
        "shell_construct" => PermissionAuthority::ShellConstruct,
        other => return Err(format!("unknown authority '{other}'")),
    };
    let target = parts.next().unwrap_or("").trim().to_string();
    if target.is_empty() {
        return Err("target is empty".to_string());
    }
    Ok(PermissionProfileRule {
        verdict,
        authority,
        target,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScriptedConsole {
        answers: std::collections::VecDeque<Answer>,
        transcript: Vec<String>,
    }

    impl ScriptedConsole {
        fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                transcript: Vec::new(),
            }
        }
    }

    impl Console for ScriptedConsole {
        fn ask(&mut self, prompt: &str) -> io::Result<Answer> {
            self.transcript.push(prompt.to_string());
            Ok(self.answers.pop_front().unwrap_or(Answer::Eof))
        }

        fn say(&mut self, line: &str) {
            self.transcript.push(line.to_string());
        }
    }

    #[test]
    fn review_maps_each_dynamic_outcome_to_its_own_terminal_safe_command() {
        fn dynamic_request(target: &str, command: &str, cwd: &str) -> newt_core::PermissionRequest {
            newt_core::PermissionRequest {
                tool: "run_command".to_string(),
                kind: DenialKind::ShellConstruct,
                target: target.to_string(),
                reason: format!(
                    "  requested shell text (untrusted): {command}\n\
                     \n  frozen working directory: {cwd}\n\
                     \n  prospective engine: brush\n\
                     \n  L3 kernel fence: Landlock (active)"
                ),
            }
        }

        let first_command = "ls -1 $(find . -name '*.rs' -type f | head -10)";
        let second_command =
            "wc -l $(find . -name '*.toml' -type f | head -5) second-command\u{202e}";
        let first = newt_core::PermissionRecord::for_request(
            "conv-review",
            &dynamic_request("blake3:first", first_command, "/work/first"),
            "allow",
            "once",
        );
        let second = newt_core::PermissionRecord::for_request(
            "conv-review",
            &dynamic_request("blake3:second", second_command, "/work/second"),
            "deny",
            "session",
        );

        let mut console = ScriptedConsole::new([]);
        render_prompt_decisions(&mut console, &[first, second]);

        let row = |needle: &str| {
            console
                .transcript
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?}: {:?}", console.transcript))
        };
        let first_outcome = row("allow once shell_construct:\"blake3:first\"");
        let first_command_row = row(first_command);
        let second_outcome = row("deny session shell_construct:\"blake3:second\"");
        let second_command_row = row("second-command");
        assert!(
            first_outcome < first_command_row
                && first_command_row < second_outcome
                && second_outcome < second_command_row,
            "each command must remain nested beneath its own outcome: {:?}",
            console.transcript
        );
        assert!(
            console
                .transcript
                .iter()
                .all(|line| !line.contains('\u{202e}')),
            "review output must not contain raw bidi controls: {:?}",
            console.transcript
        );
        assert!(
            console
                .transcript
                .iter()
                .any(|line| line.contains(r"\u{202e}")),
            "the escaped control should remain visibly auditable: {:?}",
            console.transcript
        );
    }

    #[test]
    fn escape_answer_is_back_not_exit_or_mutation() {
        let mut console = ScriptedConsole::new([Answer::Back]);
        let answer = console.ask("permission> ").unwrap();
        assert_eq!(answer, Answer::Back);
    }

    #[test]
    fn escape_does_not_alias_keep_inside_profile_fields() {
        let mut keep = ScriptedConsole::new([Answer::Line(String::new())]);
        assert_eq!(
            ask_keep(&mut keep, "Description", "old").unwrap(),
            FormValue::Value(None)
        );

        let mut back = ScriptedConsole::new([Answer::Back, Answer::Back, Answer::Back]);
        assert_eq!(
            ask_keep(&mut back, "Description", "old").unwrap(),
            FormValue::Back
        );
        assert_eq!(
            ask_optional(&mut back, "Persona", Some("coach")).unwrap(),
            FormValue::Back
        );
        assert_eq!(
            ask_scope(&mut back, "Clamp", &ScopeSpec::Keyword(ScopeKeyword::All)).unwrap(),
            FormValue::Back
        );
    }

    #[test]
    fn profile_rule_parser_covers_all_verdict_shapes() {
        assert_eq!(
            parse_profile_rule("deny exec cargo").unwrap().verdict,
            PermissionProfileVerdict::Deny
        );
        assert_eq!(
            parse_profile_rule("ask net crates.io").unwrap().authority,
            PermissionAuthority::Net
        );
        assert_eq!(
            parse_profile_rule("approve_candidate fs_write ./target")
                .unwrap()
                .verdict,
            PermissionProfileVerdict::ApproveCandidate
        );
        assert!(parse_profile_rule("allow exec cargo").is_err());
    }

    #[test]
    fn scope_parser_is_explicit_and_deterministic() {
        assert_eq!(
            parse_scope("none").unwrap(),
            ScopeSpec::Keyword(ScopeKeyword::None)
        );
        assert_eq!(
            parse_scope("src, Cargo.toml").unwrap(),
            ScopeSpec::Items(vec!["src".to_string(), "Cargo.toml".to_string()])
        );
    }

    #[test]
    fn profile_clamps_accumulate_for_prompted_remints() {
        use newt_core::CaveatsExt as _;

        let mut active = None;
        let mut coach = newt_core::Caveats::top();
        coach.exec = newt_core::Scope::none();
        accumulate_profile_clamp(&mut active, coach);

        accumulate_profile_clamp(&mut active, newt_core::Caveats::top());
        let cumulative = active.expect("a profile ceiling remains active");
        assert!(
            !cumulative.permits_exec("cargo"),
            "a later broader profile must not let a prompt pierce the earlier live-session clamp"
        );
    }

    #[test]
    fn profile_base_preset_remains_part_of_prompted_remint_ceiling() {
        use newt_core::CaveatsExt as _;

        let profile = PermissionProfile {
            schema_version: PERMISSION_PROFILE_SCHEMA_VERSION,
            name: "custom-read-only".to_string(),
            description: String::new(),
            base_preset: PermissionPreset::ReadOnly,
            prompt: true,
            persona: None,
            clamp: CaveatProfile::default(),
            rules: Vec::new(),
        };
        let requested = requested_profile_caveats(&profile, "/workspace");
        let mut active = None;
        accumulate_profile_clamp(&mut active, requested);
        // Simulate applying a later broader profile before a prompted re-mint.
        accumulate_profile_clamp(&mut active, newt_core::Caveats::top());

        let cumulative = active.expect("the custom base preset remains active");
        assert!(
            !cumulative.permits_exec("cargo"),
            "a top/default explicit clamp must not erase the read-only base's exec ceiling"
        );
        assert!(
            matches!(&cumulative.fs_write, newt_core::Scope::Only(paths) if paths.is_empty()),
            "a prompted re-mint must retain the read-only base's no-write ceiling"
        );
    }

    #[test]
    fn session_approval_refuses_target_masked_by_cumulative_profile_ceiling() {
        let mut ceiling = newt_core::Caveats::top();
        ceiling.exec = newt_core::Scope::none();
        let mut state = PermissionPromptState::default();
        let mut prompt_enabled = true;
        let mut capability = SessionCapability::establish(None, None, "/ws");
        let mut active_profiles = vec!["coach".to_string()];
        let mut active_profile_clamp = Some(ceiling.clone());
        let mut console = ScriptedConsole::new([
            Answer::Line("exec".to_string()),
            Answer::Line("cargo".to_string()),
        ]);
        let mut ctx = PermissionConsoleContext {
            state: &mut state,
            prompt_enabled: &mut prompt_enabled,
            capability: &mut capability,
            workspace: "/ws",
            config_path: None,
            key_path: None,
            log_path: None,
            conversation_id: "profile-ceiling-test",
            active_profiles: &mut active_profiles,
            active_profile_clamp: &mut active_profile_clamp,
        };

        configure_session_rule(&mut console, &mut ctx, true).unwrap();
        let transcript = console.transcript.join("\n");
        assert!(transcript.contains("Not approved"), "{transcript}");
        assert!(
            transcript.contains("cumulative permission-profile caveat ceiling"),
            "{transcript}"
        );
        assert!(!ctx
            .state
            .operator_snapshot()
            .session_grants
            .contains(&(DenialKind::Exec, "cargo".to_string())));
        assert!(
            !ceiling.permits_exec("cargo"),
            "the effective profile ceiling remains deny-only"
        );
    }

    #[test]
    fn review_lists_all_profiles_contributing_to_the_cumulative_ceiling() {
        let mut state = PermissionPromptState::default();
        let mut prompt_enabled = true;
        let mut capability = SessionCapability::establish(None, None, "/ws");
        let mut active_profiles = Vec::new();
        let mut active_profile_clamp = None;
        let mut console = ScriptedConsole::new([
            Answer::Line("coach".to_string()),
            Answer::Line("developer".to_string()),
        ]);
        let mut ctx = PermissionConsoleContext {
            state: &mut state,
            prompt_enabled: &mut prompt_enabled,
            capability: &mut capability,
            workspace: "/ws",
            config_path: None,
            key_path: None,
            log_path: None,
            conversation_id: "profile-history-test",
            active_profiles: &mut active_profiles,
            active_profile_clamp: &mut active_profile_clamp,
        };

        apply_profile_form(&mut console, &mut ctx).unwrap();
        apply_profile_form(&mut console, &mut ctx).unwrap();
        render_review(&mut console, &ctx);

        assert_eq!(ctx.active_profiles, &["coach", "developer"]);
        let transcript = console.transcript.join("\n");
        assert!(
            transcript
                .contains("Permission profiles applied (cumulative): \"coach\" -> \"developer\""),
            "{transcript}"
        );
        assert!(
            !ctx.capability.caveats().permits_exec("cargo"),
            "later developer profile cannot widen the earlier coach ceiling"
        );
    }

    #[test]
    fn profile_edit_preview_escapes_imported_terminal_format_controls() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        let profile_dir = profiles_dir(Some(&config_path)).unwrap();
        let profile = PermissionProfile {
            schema_version: PERMISSION_PROFILE_SCHEMA_VERSION,
            name: "hostile-preview".to_string(),
            description: "direction\u{202e}override".to_string(),
            base_preset: PermissionPreset::WorkspaceDev,
            prompt: true,
            persona: Some("coach\u{2066}".to_string()),
            clamp: CaveatProfile::default(),
            rules: vec![PermissionProfileRule {
                verdict: PermissionProfileVerdict::Ask,
                authority: PermissionAuthority::Exec,
                target: "cargo\u{202e}".to_string(),
                note: Some("review\u{2066}".to_string()),
            }],
        };
        newt_core::save_permission_profile(&profile_dir, &profile).unwrap();

        let mut console = ScriptedConsole::new([
            Answer::Line(profile.name.clone()),
            Answer::Line(String::new()),
            Answer::Line(String::new()),
            Answer::Line(String::new()),
            Answer::Line(String::new()),
            Answer::Line(String::new()),
            Answer::Line(String::new()),
            Answer::Line(String::new()),
            Answer::Line(String::new()),
            Answer::Line("n".to_string()),
            Answer::Line("n".to_string()),
        ]);
        let mut state = PermissionPromptState::default();
        let mut prompt_enabled = true;
        let mut capability = SessionCapability::establish(None, None, "/ws");
        let mut active_profiles = Vec::new();
        let mut active_profile_clamp = None;
        let ctx = PermissionConsoleContext {
            state: &mut state,
            prompt_enabled: &mut prompt_enabled,
            capability: &mut capability,
            workspace: "/ws",
            config_path: Some(&config_path),
            key_path: None,
            log_path: None,
            conversation_id: "preview-test",
            active_profiles: &mut active_profiles,
            active_profile_clamp: &mut active_profile_clamp,
        };

        edit_profile_form(&mut console, &ctx).unwrap();
        let transcript = console.transcript.join("\n");
        assert!(!transcript.contains('\u{202e}'), "{transcript:?}");
        assert!(!transcript.contains('\u{2066}'), "{transcript:?}");
        assert!(transcript.contains("\\u{202e}"), "{transcript:?}");
        assert!(transcript.contains("\\u{2066}"), "{transcript:?}");
        assert!(
            transcript.contains("Write this profile?"),
            "the safe preview must still reach confirmation"
        );
    }

    #[test]
    fn session_approval_reports_profile_ask_and_passkey_as_governing() {
        let mut state = PermissionPromptState::default();
        let candidates = state.apply_profile_rules(&[PermissionProfileRule {
            verdict: PermissionProfileVerdict::Ask,
            authority: PermissionAuthority::Exec,
            target: "cargo".to_string(),
            note: None,
        }]);
        assert!(candidates.is_empty());
        let snapshot = state.operator_snapshot();
        assert_eq!(
            session_approval_blocker(&snapshot, DenialKind::Exec, "cargo"),
            Some("active profile ask (must be decided at the operation)")
        );

        let (passkey, warnings) = newt_core::ocap_store::build_store(&[(
            Verdict::Passkey,
            Some("[[exec]]\ntarget = \"cargo\"\n".to_string()),
        )]);
        assert!(warnings.is_empty());
        state.apply_profile_rules(&[]);
        state.replace_ocap_policy(passkey);
        let mut snapshot = state.operator_snapshot();
        snapshot.profile_asks.clear();
        assert_eq!(
            session_approval_blocker(&snapshot, DenialKind::Exec, "cargo"),
            Some("durable passkey/step-up requirement")
        );

        let (ask, warnings) = newt_core::ocap_store::build_store(&[(
            Verdict::Ask,
            Some("[[exec]]\ntarget = \"cargo\"\n".to_string()),
        )]);
        assert!(warnings.is_empty());
        snapshot.ocap_policy = ask;
        assert_eq!(
            session_approval_blocker(&snapshot, DenialKind::Exec, "cargo"),
            None,
            "an explicit session approval intentionally overrides durable Ask"
        );
    }
}
