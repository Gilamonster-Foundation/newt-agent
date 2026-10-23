//! #2933: the doer soul as named components.
//!
//! [`DEFAULT_SOUL`](crate::DEFAULT_SOUL) is one 5 KB string whose rules cannot be
//! judged, shown or dropped one at a time. Here each rule is a [`Component`] with an
//! id, a description and the tools it needs; [`render_soul`] joins the ones that apply.
//! With every component on and every tool visible the result is `DEFAULT_SOUL` byte for
//! byte (pinned by the tests), so nothing changes until a later PR changes a default.
//!
//! Dropping a component is presentation only: it changes what the model is told, never
//! what dispatch permits.

/// The tools the `tools` component can name, in the order the soul has always named them.
pub const NAMED_TOOLS: &[&str] = &[
    "run_command",
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "find",
    "use_skill",
    "web_fetch",
    "render_report",
];

/// The prefix of the sentence that enumerates the tools.
pub const TOOL_LINE_PREFIX: &str = "You have tools: ";

/// Printed once, before the first rule that sits under it.
const HEADING: &str = "## How to work";

/// What a component renders.
#[derive(Debug, Clone, Copy)]
enum Body {
    Text(&'static str),
    /// The `You have tools:` sentence over the visible subset of [`NAMED_TOOLS`].
    Tools,
}

/// One rule of the soul.
#[derive(Debug, Clone, Copy)]
pub struct Component {
    pub id: &'static str,
    pub description: &'static str,
    /// Tools the text names. The component is dropped when any is hidden, so the
    /// prompt never names a tool the operator took away.
    pub requires_tools: &'static [&'static str],
    pub default_on: bool,
    /// Rules under `## How to work`; the identity lines sit above it.
    under_heading: bool,
    body: Body,
}

impl Component {
    fn render(&self, visible: &dyn Fn(&str) -> bool) -> String {
        match self.body {
            Body::Text(text) => text.to_owned(),
            Body::Tools => {
                let names: Vec<&str> = NAMED_TOOLS.iter().copied().filter(|t| visible(t)).collect();
                format!("{TOOL_LINE_PREFIX}{}.", names.join(", "))
            }
        }
    }
}

const IDENTITY: &str =
    "You are newt, a free, friendly, local agentic coder. Be concise and direct.";

const DISPOSITION: &str = "Follow the per-turn disposition card supplied by the harness: only an `act` turn may mutate the workspace or receive execution pressure; `ask` asks its bounded clarification and stops, `explain` answers without mutation, `research` gathers bounded read-only evidence, and `plan` may update only the harness-owned plan ledger.";

const ONE_CHANGE: &str = "\
**One change at a time.** Read only the files you need for the immediate next step. \
Make the change. Commit it. Then move to the next step. \
Never accumulate multiple uncommitted edits — a committed partial result survives a crash; \
an uncommitted complete result does not.";

const READ_MINIMUM: &str = "\
**Read minimum, act deliberately.** Resist reading the entire codebase before acting. \
Read the specific file or function you are about to change, make the change, \
commit, then read the next thing. The session has a finite context window — \
every token spent reading is a token not spent writing.";

const PREFER_EDIT: &str = "\
**Prefer edit_file over write_file.** For any existing file, use edit_file \
to replace a specific string — you only generate the change, not the whole file. \
Only use write_file when creating a new file or when you have generated the \
complete contents in full. write_file will refuse if the new content is \
significantly shorter than the original.";

const STOP_WHEN_BLOCKED: &str = "\
**Stop when blocked.** If the same tool call fails twice in a row with the \
same error, stop immediately and tell the user what blocked you and why. \
Do not try alternative installation methods, do not loop, do not pivot to \
answering a different question. Two identical failures are a signal to report, \
not to retry. One sentence explaining the block is worth more than ten more \
failed tool calls.";

const GROUND_TRUTH: &str = "\
**Seek ground truth.** After every action, verify what actually happened — \
not what you intended. Do not proceed on assumptions about your own actions; \
confirm them. After writing a file, the tool reports the new line count — \
check it matches what you expected. After editing code, the tool reports \
whether it compiled — if it did not, fix the error before committing. \
Before committing, confirm you are on the right branch. \
A belief that something worked is worthless; a tool result that confirms it \
is ground truth.";

const COMMIT_ATTRIBUTION: &str = "\
**Sign every commit.** Credit yourself on every commit you author with a \
`Co-authored-by:` trailer naming the model and harness, e.g. \
`Co-authored-by: <model> (v<version> <build>) <309460085+newt-agent@users.noreply.github.com>`. \
The account name is not repeated in the parenthetical — the address already \
carries it, so the qualifier holds only the harness version and the build it \
was compiled from. \
The harness identity is the newt-agent GitHub App — link it: \
<https://github.com/apps/newt-agent>. This is the house default, not an \
occasional courtesy — include it on every commit, including commits made \
through the shell `git` path (which bypasses the harness's automatic \
trailer). When you amend or reword, keep the trailer.";

const ACT_NOT_DESCRIBE: &str = "\
**On an `act` turn, never describe a code change — make it.** Do not paste code into the chat. \
If the task requires a code change and the disposition is `act`, call edit_file or write_file immediately. \
A markdown code block in the conversation is invisible to the filesystem — \
it does not modify any file. Write the code once, into the file, via the tool. \
Showing code in text is NOT completing an `act` task; calling the tool IS. \
For `ask`, `explain`, `research`, or `plan`, respect the disposition instead of forcing a workspace write.";

const PRESENT_FINDINGS: &str = "\
**Present findings — don't just report a blocker.** When the task is to \
gather or summarize (a status roll-up, a triage sweep, a morning briefing), \
your deliverable is a rendered report, not a one-line status. The moment you \
have what was asked, call render_report to present it. A failed data source \
is one degraded section, not a dead end — render the rest and mark the failed \
part `degraded` (or `error`), so the human sees the partial result plus \
exactly what is missing. Ending such a task with only \"X is broken\" leaves \
the work you already did invisible.";

const EXPLORE_BUDGET: &str = "\
**Exploration budget for `act`.** Treat read-only rounds (list_dir, read_file) as expensive. \
Spend at most three consecutive rounds on exploration before making a write. \
Once you have read the file you need, stop reading and call edit_file or write_file. \
Continued reading without writing means you are lost — make your best attempt \
at the change based on what you have already read, then verify. In `research`, \
the bounded read-only evidence collection is the work; report it rather than \
forcing a mutation.";

const THREE_CS: &str = "\
**Working code first, then the three Cs.** Make it work, then make it right. \
Shipping a working result that hardcodes a list or a constant to get there is \
fine; functional results come first. Then RETURN to the three Cs: lift hardcoded \
knowledge (keyword lists, magic values, language or domain rules) into pure DATA \
that is Composed, Configured, and Convention-driven, so a new case is config, \
not code. Don't let this block shipping; do circle back and de-hardcode once it \
works.";

/// The soul, in the order it reads.
pub const SOUL: &[Component] = &[
    Component {
        id: "identity",
        description: "Who the agent is: name and tone, one line.",
        requires_tools: &[],
        default_on: true,
        under_heading: false,
        body: Body::Text(IDENTITY),
    },
    Component {
        id: "tools",
        description: "The `You have tools:` line, generated from the tools the operator exposes.",
        requires_tools: &[],
        default_on: true,
        under_heading: false,
        body: Body::Tools,
    },
    Component {
        id: "disposition",
        description: "Tells the model to follow the per-turn disposition card (act / ask / explain / research / plan).",
        requires_tools: &[],
        default_on: true,
        under_heading: false,
        body: Body::Text(DISPOSITION),
    },
    Component {
        id: "habit-one-change",
        description: "One change at a time: read little, change, commit, repeat.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        body: Body::Text(ONE_CHANGE),
    },
    Component {
        id: "habit-read-minimum",
        description: "Read only what the next step needs; context is finite.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        body: Body::Text(READ_MINIMUM),
    },
    Component {
        id: "habit-prefer-edit",
        description: "Prefer edit_file over write_file for existing files.",
        requires_tools: &["edit_file", "write_file"],
        default_on: true,
        under_heading: true,
        body: Body::Text(PREFER_EDIT),
    },
    Component {
        id: "habit-stop-when-blocked",
        description: "Stop and report after the same tool call fails twice.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        body: Body::Text(STOP_WHEN_BLOCKED),
    },
    Component {
        id: "habit-ground-truth",
        description: "Verify what actually happened after every action.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        body: Body::Text(GROUND_TRUTH),
    },
    Component {
        id: "commit-attribution",
        description: "Credit the model and harness in a Co-authored-by trailer on every commit.",
        requires_tools: &["git"],
        default_on: true,
        under_heading: true,
        body: Body::Text(COMMIT_ATTRIBUTION),
    },
    Component {
        id: "habit-act-not-describe",
        description: "On an act turn, make the change with a tool instead of pasting code.",
        requires_tools: &["edit_file", "write_file"],
        default_on: true,
        under_heading: true,
        body: Body::Text(ACT_NOT_DESCRIBE),
    },
    Component {
        id: "habit-present-findings",
        description: "Present gathered findings with render_report, degraded sections included.",
        requires_tools: &["render_report"],
        default_on: true,
        under_heading: true,
        body: Body::Text(PRESENT_FINDINGS),
    },
    Component {
        id: "habit-explore-budget",
        description: "At most three read-only rounds before an act-turn write.",
        requires_tools: &["list_dir", "read_file", "edit_file", "write_file"],
        default_on: true,
        under_heading: true,
        body: Body::Text(EXPLORE_BUDGET),
    },
    Component {
        id: "three-cs",
        description: "Working code first, then lift hardcoded knowledge into composed, configured data.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        body: Body::Text(THREE_CS),
    },
];

/// The soul with the components `enabled` selects and the rules whose tools are all
/// `visible`. Lines above the heading are joined by a space, rules by a blank line.
pub fn render_soul(enabled: &dyn Fn(&Component) -> bool, visible: &dyn Fn(&str) -> bool) -> String {
    let (mut lead, mut rules) = (Vec::new(), Vec::new());
    for c in SOUL
        .iter()
        .filter(|c| enabled(c) && c.requires_tools.iter().all(|t| visible(t)))
    {
        if c.under_heading {
            rules.push(c.render(visible));
        } else {
            lead.push(c.render(visible));
        }
    }
    let mut out = lead.join(" ");
    if !rules.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(HEADING);
        out.push_str("\n\n");
        out.push_str(&rules.join("\n\n"));
    }
    out
}

/// The default preset: every component whose `default_on` is set.
#[must_use]
pub fn default_soul(visible: &dyn Fn(&str) -> bool) -> String {
    render_soul(&|c| c.default_on, visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(_: &str) -> bool {
        true
    }

    /// #2933: the default components ARE the soul. The const stays as the oracle until
    /// the soul is retired.
    #[test]
    fn default_components_join_to_default_soul() {
        assert_eq!(default_soul(&all), crate::DEFAULT_SOUL);
    }

    #[test]
    fn ids_are_unique_described_and_kebab_case() {
        let mut seen = std::collections::BTreeSet::new();
        for c in SOUL {
            assert!(seen.insert(c.id), "duplicate id `{}`", c.id);
            assert!(
                !c.description.trim().is_empty(),
                "`{}` has no description",
                c.id
            );
            assert!(
                c.id.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "`{}` is not kebab-case",
                c.id
            );
        }
        assert_eq!(SOUL.len(), 13);
    }

    #[test]
    fn every_component_is_on_by_default() {
        assert!(SOUL.iter().all(|c| c.default_on));
    }

    #[test]
    fn the_tools_line_names_only_visible_tools() {
        let text = render_soul(&|c| c.id == "tools", &|t| t == "find" || t == "read_file");
        assert_eq!(text, "You have tools: read_file, find.");
    }

    #[test]
    fn the_heading_goes_with_the_first_rule_and_never_alone() {
        let lead_only = render_soul(&|c| c.id == "identity", &all);
        assert!(!lead_only.contains("## How to work"));
        let one_rule = render_soul(&|c| c.id == "three-cs", &all);
        assert!(one_rule.starts_with("## How to work\n\n**Working code first"));
    }

    #[test]
    fn a_rule_is_dropped_when_its_tool_is_hidden() {
        let text = default_soul(&|t| t != "render_report");
        assert!(!text.contains("Present findings"));
        assert!(text.contains("Prefer edit_file"));
    }
}
