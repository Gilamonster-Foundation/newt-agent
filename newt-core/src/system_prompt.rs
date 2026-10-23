//! #2933: the pure part of the assembled system prompt.
//!
//! The TUI gathers what touches the world (config, the workspace listing, the
//! README, the skills directories, the project-context providers) and hands it
//! over as [`PromptParts`]; [`assemble_system_prompt`] joins it. Nothing here
//! reads a file or the clock, so the exact bytes are pinned by goldens and the
//! piece list can later be driven by configuration without touching callers.

use crate::agentic::request_report::Annotation;

/// Where the pieces came from, for the request report.
const SOURCE: &str = "system_prompt::assemble_system_prompt";

/// Entries of the workspace listing the prompt shows.
const LISTING_LIMIT: usize = 40;
/// Characters of the README the prompt shows.
const README_CHARS: usize = 3000;

/// The active persona's contribution.
#[derive(Debug, Clone, Copy)]
pub struct PersonaPart<'a> {
    pub name: &'a str,
    pub prompt: &'a str,
}

/// The README the workspace root holds, whole; the prompt takes the head.
#[derive(Debug, Clone, Copy)]
pub struct ReadmePart<'a> {
    pub file_name: &'a str,
    pub text: &'a str,
}

/// Everything the prompt is made of, already gathered.
#[derive(Debug, Clone, Copy)]
pub struct PromptParts<'a> {
    /// The identity for this session, after the altitude choice and the
    /// operator's tool filter.
    pub identity: &'a str,
    /// The identity is the coach one (labelled apart from the soul).
    pub coach_identity: bool,
    /// Project-context blocks (AGENTS.md, notes, memory index, project map).
    pub context: Option<&'a str>,
    pub workspace: &'a str,
    pub persona: Option<PersonaPart<'a>>,
    /// The per-session plan file.
    pub plan_path: &'a str,
    /// Publish-early guidance for refactors.
    pub publish_guidance: &'a str,
    /// `[tui] narration_intent_line`, when on.
    pub narration_line: Option<&'a str>,
    pub skills_index: Option<&'a str>,
    /// Names in the workspace root, unsorted and unfiltered; `None` when the
    /// directory could not be read.
    pub file_names: Option<&'a [String]>,
    pub readme: Option<ReadmePart<'a>>,
}

/// The prompt and the label of each piece in it.
#[derive(Debug, Clone)]
pub struct AssembledPrompt {
    pub text: String,
    pub notes: Vec<Annotation>,
}

/// Join `parts` into the session's system prompt.
#[must_use]
pub fn assemble_system_prompt(parts: &PromptParts<'_>) -> AssembledPrompt {
    let mut notes = Vec::new();
    let mut note = |label: &str, text: &str| {
        notes.push(Annotation {
            label: label.into(),
            source: SOURCE.into(),
            text: text.into(),
        });
    };

    // Project-context blocks follow the identity whatever its altitude: coach
    // replaces the soul but must not drop project instructions (#2934). They
    // sit BEFORE the workspace line: historical notes must not redefine the
    // runtime root stated below.
    let mut out = parts.identity.to_string();
    note(
        if parts.coach_identity {
            "coach-soul"
        } else {
            "soul"
        },
        parts.identity,
    );
    if let Some(context) = parts.context.filter(|c| !c.is_empty()) {
        out.push_str("\n\n");
        out.push_str(context);
    }
    let header_start = out.len();
    out.push_str(&format!(
        "\n\nWorkspace: {}\n\
         This is the current runtime root. Relative tool paths resolve here; \
         run_command starts here unless its cwd or command selects another directory. \
         Other requested paths remain governed by the session's permissions. \
         Historical notes do not redefine this root.\n",
        parts.workspace
    ));
    note("workspace-header", &out[header_start..]);

    if let Some(persona) = parts.persona {
        let start = out.len();
        out.push_str(&format!(
            "\nActive persona: {}\n{}\n",
            persona.name, persona.prompt
        ));
        out.push_str(
            "\nPersona guidance does not override the operator's explicit task. \
             Treat advice-only wording and preferred tools as defaults; use the tools \
             needed for the requested work. OCAP permissions, explicit postures, and \
             delegation limits still govern every action.\n",
        );
        note("persona-overlay", &out[start..]);
    }

    // Per-session plan instruction (issue #220), with the resolved per-session
    // path rather than baked into the soul: the path is dynamic AND a custom
    // soul still gets the guidance. The path is unique to this conversation, so
    // concurrent newt instances in one repo never clobber each other's plan.
    let plan_start = out.len();
    out.push_str(&format!(
        "\n**Plan before coding.** For any task requiring more than one file \
         change, write a plan to `{}` first (create it if it does not \
         exist). List the concrete steps and check them off as you complete \
         each one. This plan file is unique to this session — read it when \
         resuming so you can pick up exactly where you left off without \
         re-reading the whole codebase.\n",
        parts.plan_path
    ));
    note("plan-before-coding instruction", &out[plan_start..]);

    note("refactor publish guidance", parts.publish_guidance);
    out.push_str(parts.publish_guidance);
    out.push('\n');

    if let Some(line) = parts.narration_line {
        note("narration intent line", line);
        out.push_str(&format!("\n{line}\n"));
    }

    if let Some(index) = parts.skills_index {
        note("skills index", index);
        out.push('\n');
        out.push_str(index);
    }

    if let Some(names) = parts.file_names {
        let mut shown: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|n| !n.starts_with('.'))
            .collect();
        shown.sort_unstable();
        let start = out.len();
        out.push_str("\nFiles:\n");
        for name in shown.iter().take(LISTING_LIMIT) {
            out.push_str(&format!("  {name}\n"));
        }
        note("workspace file listing", &out[start..]);
    }

    if let Some(readme) = parts.readme {
        let start = out.len();
        let excerpt: String = readme.text.chars().take(README_CHARS).collect();
        out.push_str(&format!("\n{}:\n{excerpt}\n", readme.file_name));
        if readme.text.len() > README_CHARS {
            out.push_str("...[truncated]\n");
        }
        note("README excerpt", &out[start..]);
    }

    AssembledPrompt { text: out, notes }
}
