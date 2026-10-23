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

/// The coach altitude's heading, printed once before its first rule.
const COACH_HEADING: &str = "## How to coach";

/// Printed once, before the first rule that sits under it.
const HEADING: &str = "## How to work";

/// What a component renders.
#[derive(Debug, Clone, Copy)]
enum Body {
    Text(&'static str),
    /// The `You have tools:` sentence over the visible subset of [`NAMED_TOOLS`].
    Tools,
    /// Text the harness produces each turn; the registry only names and gates it.
    Owned,
}

/// Where in the request a component sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// The frozen system prompt's identity and rules.
    Soul,
    /// The rest of the frozen system prompt: project context, header, listings.
    Frozen,
    /// Rebuilt every turn into the turn's system message.
    Turn,
    /// Added by the agentic driver as its own system message.
    Driver,
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
    pub layer: Layer,
    /// Belongs to the coach altitude's soul, not the doer's.
    coach: bool,
    /// The text under preset `pi`, when it differs from `body`'s.
    pi_text: Option<&'static str>,
    /// Describes a fence the harness enforces elsewhere. Switching it off only stops
    /// telling the model about the fence; enforcement does not read this.
    pub fence: bool,
    body: Body,
}

impl Component {
    /// Whether this belongs to the coach altitude's soul rather than the doer's.
    #[must_use]
    pub fn is_coach(&self) -> bool {
        self.coach
    }

    fn render(&self, visible: &dyn Fn(&str) -> bool, preset: Preset) -> String {
        match self.body {
            Body::Text(text) => match (preset, self.pi_text) {
                (Preset::Pi, Some(pi)) => pi.to_owned(),
                _ => text.to_owned(),
            },
            Body::Owned => String::new(),
            Body::Tools => {
                let names: Vec<&str> = NAMED_TOOLS.iter().copied().filter(|t| visible(t)).collect();
                format!("{TOOL_LINE_PREFIX}{}.", names.join(", "))
            }
        }
    }
}

/// The one identity line of preset `pi`.
const PI_IDENTITY: &str = "You are newt, a coding agent.";

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
        layer: Layer::Soul,
        coach: false,
        pi_text: Some(PI_IDENTITY),
        fence: false,
        body: Body::Text(IDENTITY),
    },
    Component {
        id: "tools",
        description: "The `You have tools:` line, generated from the tools the operator exposes.",
        requires_tools: &[],
        default_on: true,
        under_heading: false,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Tools,
    },
    Component {
        id: "disposition",
        description: "Tells the model to follow the per-turn disposition card (act / ask / explain / research / plan).",
        requires_tools: &[],
        default_on: true,
        under_heading: false,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(DISPOSITION),
    },
    Component {
        id: "habit-one-change",
        description: "One change at a time: read little, change, commit, repeat.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(ONE_CHANGE),
    },
    Component {
        id: "habit-read-minimum",
        description: "Read only what the next step needs; context is finite.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(READ_MINIMUM),
    },
    Component {
        id: "habit-prefer-edit",
        description: "Prefer edit_file over write_file for existing files.",
        requires_tools: &["edit_file", "write_file"],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(PREFER_EDIT),
    },
    Component {
        id: "habit-stop-when-blocked",
        description: "Stop and report after the same tool call fails twice.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(STOP_WHEN_BLOCKED),
    },
    Component {
        id: "habit-ground-truth",
        description: "Verify what actually happened after every action.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(GROUND_TRUTH),
    },
    Component {
        id: "commit-attribution",
        description: "Credit the model and harness in a Co-authored-by trailer on every commit.",
        requires_tools: &["git"],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(COMMIT_ATTRIBUTION),
    },
    Component {
        id: "habit-act-not-describe",
        description: "On an act turn, make the change with a tool instead of pasting code.",
        requires_tools: &["edit_file", "write_file"],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(ACT_NOT_DESCRIBE),
    },
    Component {
        id: "habit-present-findings",
        description: "Present gathered findings with render_report (now the `present-findings` skill; off by default).",
        requires_tools: &["render_report"],
        default_on: false,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(PRESENT_FINDINGS),
    },
    Component {
        id: "habit-explore-budget",
        description: "At most three read-only rounds before an act-turn write.",
        requires_tools: &["list_dir", "read_file", "edit_file", "write_file"],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(EXPLORE_BUDGET),
    },
    Component {
        id: "three-cs",
        description: "Working code first, then lift hardcoded knowledge into data (now the `three-cs` skill; off by default).",
        requires_tools: &[],
        default_on: false,
        under_heading: true,
        layer: Layer::Soul,
        coach: false,
        pi_text: None,
        fence: false,
        body: Body::Text(THREE_CS),
    },
];

const COACH_IDENTITY: &str = "\
You are newt in COACH mode: an advisor, not a doer. Be concise and direct. \
You help the human reason about their code and infrastructure — you explain, \
present options, and recommend the next step. You do NOT make the change \
yourself.";

const COACH_ADVISE: &str = "\
**Advise; do not act.** Present the command, the edit, or the plan as text, \
with the reasoning behind it, and let the human decide and execute. A coaching \
turn's product is UNDERSTANDING and a recommendation — never a mutated file or \
an executed command. Do not call write_file, edit_file, or a state-changing \
run_command; if the change needs making, say so and let the human make it (or \
switch out of coach mode).";

const COACH_GROUND: &str = "\
**Ground your advice in the real code.** Use the read-only tools — read_file, \
list_dir, find, web_fetch — to see what is actually there before you advise. \
Advice built on an assumption about the code is worse than none; confirm \
against ground truth, then recommend.";

const COACH_SHOW_COMMAND: &str = "\
**Show the command, don't run it.** When the answer is 'run X', write X in the \
reply with a one-line explanation of what it does and why — as something the \
human runs, not something you run. A command in the conversation is guidance; \
executing it would be doing the human's job for them.";

const COACH_TEACH: &str = "\
**Teach the reasoning, not just the answer.** Explain WHY, name the trade-offs, \
and point at the one next step. The human is learning from you, not delegating \
to you — leave them able to make the next call themselves.";

const COACH_STOP_UNSURE: &str = "\
**Stop when unsure.** If you cannot ground a recommendation, say what you would \
need to check rather than guessing. One honest 'here is what I'd verify first' \
beats a confident wrong answer.";

/// The coach altitude's soul as components (#2933): chosen by the persona's altitude
/// in place of the doer's, on the same rails (ids, `[prompt] components`, the listing).
/// All on, they join to [`COACH_SOUL`](crate::COACH_SOUL), the oracle the tests keep.
pub const COACH: &[Component] = &[
    Component {
        id: "coach-identity",
        description: "Coach altitude: who the agent is (an advisor, not a doer).",
        requires_tools: &[],
        default_on: true,
        under_heading: false,
        layer: Layer::Soul,
        coach: true,
        pi_text: None,
        fence: false,
        body: Body::Text(COACH_IDENTITY),
    },
    Component {
        id: "coach-advise",
        description: "Coach altitude: advise, do not act; present the change as text.",
        requires_tools: &["write_file", "edit_file", "run_command"],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: true,
        pi_text: None,
        fence: false,
        body: Body::Text(COACH_ADVISE),
    },
    Component {
        id: "coach-ground",
        description: "Coach altitude: ground the advice in the real code, with read-only tools.",
        requires_tools: &["read_file", "list_dir", "find", "web_fetch"],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: true,
        pi_text: None,
        fence: false,
        body: Body::Text(COACH_GROUND),
    },
    Component {
        id: "coach-show-command",
        description: "Coach altitude: show the command, do not run it.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: true,
        pi_text: None,
        fence: false,
        body: Body::Text(COACH_SHOW_COMMAND),
    },
    Component {
        id: "coach-teach",
        description: "Coach altitude: teach the reasoning, not just the answer.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: true,
        pi_text: None,
        fence: false,
        body: Body::Text(COACH_TEACH),
    },
    Component {
        id: "coach-stop-unsure",
        description: "Coach altitude: stop and say what to verify when unsure.",
        requires_tools: &[],
        default_on: true,
        under_heading: true,
        layer: Layer::Soul,
        coach: true,
        pi_text: None,
        fence: false,
        body: Body::Text(COACH_STOP_UNSURE),
    },
];

/// The harness-produced pieces of the per-turn system message and the driver's
/// active-prompt card (#2933). Their text is built where the session state lives;
/// the registry names them, describes them and gates them.
const fn owned(
    id: &'static str,
    description: &'static str,
    layer: Layer,
    fence: bool,
) -> Component {
    Component {
        id,
        description,
        requires_tools: &[],
        default_on: true,
        under_heading: false,
        layer,
        coach: false,
        pi_text: None,
        fence,
        body: Body::Owned,
    }
}

/// The per-turn pieces, in the order the turn's system message carries them.
pub const TURN: &[Component] = &[
    owned(
        "workspace-state",
        "Branch, dirty files and recent commits, refreshed every turn.",
        Layer::Turn,
        false,
    ),
    owned(
        "environment",
        "Harness version, model, backend, date and the rule to never invent an identity.",
        Layer::Turn,
        false,
    ),
    owned(
        "runtime-authority",
        "Tells the model when a runtime authority (e.g. full access) is active.",
        Layer::Turn,
        true,
    ),
    owned(
        "git-identity",
        "The configured Git identity and how commit attribution is recorded.",
        Layer::Turn,
        false,
    ),
    owned(
        "fs-confinement",
        "Tells the model which paths it can read and write.",
        Layer::Turn,
        true,
    ),
    owned(
        "operating-mode",
        "The session's operating mode and permission posture.",
        Layer::Turn,
        false,
    ),
];

/// The driver's own pieces. `card` may be off only while the tools that read it are
/// hidden from the model ([`ComponentSet::check_card`]).
pub const DRIVER: &[Component] = &[owned(
    "card",
    "The protected active-prompt card the model re-reads with prompt_read / artifact_read.",
    Layer::Driver,
    false,
)];

/// The rest of the frozen system prompt (#2933): project context, the workspace
/// header, the session plan, the skills index and the directory listing. The text is
/// gathered by the TUI and joined by [`crate::system_prompt`]; the registry names,
/// describes and gates each piece.
pub const FROZEN: &[Component] = &[
    owned(
        "project-instructions",
        "The AGENTS.md / CLAUDE.md instructions block.",
        Layer::Frozen,
        false,
    ),
    owned(
        "notes",
        "The agent's saved notes (`## Agent Notes`).",
        Layer::Frozen,
        false,
    ),
    owned(
        "memory-index",
        "The index of saved memories.",
        Layer::Frozen,
        false,
    ),
    owned(
        "project-map",
        "A map of the project's files and symbols.",
        Layer::Frozen,
        false,
    ),
    owned(
        "api-surface",
        "The public API / FFI surface of the project.",
        Layer::Frozen,
        false,
    ),
    owned(
        "persona",
        "The active persona's overlay and its guidance sentence.",
        Layer::Frozen,
        false,
    ),
    owned(
        "workspace-header",
        "The workspace root and how relative paths resolve.",
        Layer::Frozen,
        false,
    ),
    owned(
        "plan-file",
        "The instruction to write a plan to the session's plan file.",
        Layer::Frozen,
        false,
    ),
    owned(
        "publish-guidance",
        "Publish-early guidance for refactors (worktree, push, PR).",
        Layer::Frozen,
        false,
    ),
    owned(
        "skills-index",
        "One line per installed skill.",
        Layer::Frozen,
        false,
    ),
    owned(
        "file-listing",
        "The workspace root's file listing.",
        Layer::Frozen,
        false,
    ),
    owned(
        "readme",
        "The first 3,000 characters of the README.",
        Layer::Frozen,
        false,
    ),
];

/// The context provider whose block is the component of the same name, if any.
#[must_use]
pub fn provider_component(provider: &str) -> Option<&'static str> {
    match provider {
        "agents" => Some("project-instructions"),
        "note_store" => Some("notes"),
        "memory_index" => Some("memory-index"),
        "project_map" => Some("project-map"),
        "api_surface" | "knowledge_base" => Some("api-surface"),
        _ => None,
    }
}

/// A named set of components to start from (`[prompt] preset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// Everything on: today's prompt.
    #[default]
    Default,
    /// A pi-like minimum: one identity line, the tool list, the workspace header,
    /// project instructions and the persona.
    Pi,
}

impl Preset {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Pi => "pi",
        }
    }

    /// The components that are on under this preset.
    fn is_on(self, c: &Component) -> bool {
        match self {
            Self::Default => c.default_on,
            Self::Pi => PI_ON.contains(&c.id),
        }
    }
}

/// What preset `pi` keeps; everything else is off, `card` included.
pub const PI_ON: &[&str] = &[
    "identity",
    "coach-identity",
    "tools",
    "workspace-header",
    "project-instructions",
    "persona",
];

/// The tools that read the active-prompt card.
pub const CARD_TOOLS: &[&str] = &["prompt_read", "artifact_read"];

/// Every component: the soul, the rest of the frozen prompt, the turn's pieces, then
/// the driver's.
pub fn all() -> impl Iterator<Item = &'static Component> {
    SOUL.iter()
        .chain(COACH)
        .chain(FROZEN)
        .chain(TURN)
        .chain(DRIVER)
}

/// The soul with the components `enabled` selects and the rules whose tools are all
/// `visible`. Lines above the heading are joined by a space, rules by a blank line.
pub fn render_soul(
    enabled: &dyn Fn(&Component) -> bool,
    visible: &dyn Fn(&str) -> bool,
    preset: Preset,
    coach: bool,
) -> String {
    let parts = soul_parts(enabled, visible, preset, coach);
    let mut out = String::new();
    for (n, (id, text)) in parts.iter().enumerate() {
        if n > 0 {
            out.push_str(if under_heading(id) { "\n\n" } else { " " });
        }
        out.push_str(text);
    }
    out
}

fn under_heading(id: &str) -> bool {
    SOUL.iter()
        .chain(COACH)
        .any(|c| c.id == id && c.under_heading)
}

/// The pieces [`render_soul`] joins, each with its component id, in order. The
/// `## How to work` heading rides with the first rule, so every byte of the soul
/// but the joins belongs to one component (the request report's attribution).
pub fn soul_parts(
    enabled: &dyn Fn(&Component) -> bool,
    visible: &dyn Fn(&str) -> bool,
    preset: Preset,
    coach: bool,
) -> Vec<(&'static str, String)> {
    let mut parts: Vec<(&'static str, String)> = Vec::new();
    let mut headed = false;
    let heading = if coach { COACH_HEADING } else { HEADING };
    for c in SOUL
        .iter()
        .chain(COACH)
        .filter(|c| c.coach == coach)
        .filter(|c| enabled(c) && c.requires_tools.iter().all(|t| visible(t)))
    {
        let mut text = c.render(visible, preset);
        if c.under_heading && !headed {
            headed = true;
            text = format!("{heading}\n\n{text}");
        }
        parts.push((c.id, text));
    }
    parts
}

/// The default preset: every component whose `default_on` is set.
#[must_use]
pub fn default_soul(visible: &dyn Fn(&str) -> bool) -> String {
    render_soul(&|c| c.default_on, visible, Preset::Default, false)
}

/// Presets `[prompt] preset` accepts. `headless` is the headless-only bare message,
/// not a TUI preset.
pub const PRESETS: &[&str] = &["default", "pi"];

/// Every component id, in prompt order.
#[must_use]
pub fn ids() -> Vec<&'static str> {
    all().map(|c| c.id).collect()
}

/// Which components are on: a preset's defaults with the operator's deltas applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSet {
    preset: Preset,
    off: std::collections::BTreeSet<&'static str>,
}

impl Default for ComponentSet {
    /// The `default` preset: everything whose `default_on` is set.
    fn default() -> Self {
        Self::for_preset(Preset::Default)
    }
}

impl ComponentSet {
    /// `preset` untouched.
    #[must_use]
    pub fn for_preset(preset: Preset) -> Self {
        Self {
            preset,
            off: all().filter(|c| !preset.is_on(c)).map(|c| c.id).collect(),
        }
    }

    /// The preset this set started from.
    #[must_use]
    pub fn preset(&self) -> Preset {
        self.preset
    }

    /// `preset` (absent = `default`) plus `[prompt] components` deltas. Refuses an
    /// unknown preset or id, naming the valid ones, and an id in both lists.
    pub fn resolve(preset: Option<&str>, on: &[String], off: &[String]) -> Result<Self, String> {
        let preset = match preset.map(str::trim) {
            None | Some("default") => Preset::Default,
            Some("pi") => Preset::Pi,
            Some("headless") => {
                return Err(format!(
                    "`headless` is the headless-only preset (`newt headless --prompt-preset`), \
                     not a [prompt] preset; presets: {}",
                    PRESETS.join(", ")
                ))
            }
            Some(other) => {
                return Err(format!(
                    "unknown [prompt] preset `{other}`; presets: {}",
                    PRESETS.join(", ")
                ))
            }
        };
        let known = |id: &str| -> Result<&'static str, String> {
            all().map(|c| c.id).find(|c| *c == id).ok_or_else(|| {
                format!(
                    "unknown [prompt] component `{id}`; components: {}",
                    ids().join(", ")
                )
            })
        };
        let mut set = Self::for_preset(preset);
        for id in off {
            if on.contains(id) {
                return Err(format!(
                    "[prompt] component `{id}` is in both `on` and `off`"
                ));
            }
            set.off.insert(known(id)?);
        }
        for id in on {
            set.off.remove(known(id)?);
        }
        Ok(set)
    }

    #[must_use]
    pub fn is_on(&self, c: &Component) -> bool {
        !self.off.contains(c.id)
    }

    /// [`is_on`](Self::is_on) by id; an unknown id reads as on.
    #[must_use]
    pub fn is_on_id(&self, id: &str) -> bool {
        !self.off.contains(id)
    }

    /// Whether this is the `default` preset untouched, i.e. today's bytes.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// One line of the component listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: &'static str,
    pub on: bool,
    /// Characters it adds to the prompt when on (0 when its tools are hidden);
    /// `None` for a piece the harness builds each turn, whose size varies.
    pub chars: Option<usize>,
    pub layer: Layer,
    /// Describes an enforced fence: off only stops telling the model.
    pub fence: bool,
    pub description: &'static str,
    /// The first required tool that is hidden, which keeps the rule out.
    pub needs: Option<&'static str>,
}

/// The listing `/settings system-prompt components` shows.
#[must_use]
pub fn rows(set: &ComponentSet, visible: &dyn Fn(&str) -> bool) -> Vec<Row> {
    all()
        .map(|c| {
            let needs = c.requires_tools.iter().copied().find(|t| !visible(t));
            let chars = if needs.is_some() {
                Some(0)
            } else {
                // Sized from the selected preset's rendering, the one the prompt uses.
                match c.body {
                    Body::Text(_) | Body::Tools => {
                        Some(c.render(visible, set.preset()).chars().count())
                    }
                    Body::Owned => None,
                }
            };
            Row {
                id: c.id,
                on: set.is_on(c),
                chars,
                layer: c.layer,
                fence: c.fence,
                description: c.description,
                needs,
            }
        })
        .collect()
}

impl ComponentSet {
    /// The set with `id` switched on or off. Refuses an unknown id, naming the valid ones.
    pub fn with(&self, id: &str, on: bool) -> Result<Self, String> {
        let known = all()
            .find(|c| c.id == id)
            .ok_or_else(|| format!("unknown component `{id}`; components: {}", ids().join(", ")))?;
        let mut next = self.clone();
        if on {
            next.off.remove(known.id);
        } else {
            next.off.insert(known.id);
        }
        Ok(next)
    }

    /// `(on, off)` relative to this set's own preset: what `[prompt] components` stores.
    #[must_use]
    pub fn deltas(&self) -> (Vec<&'static str>, Vec<&'static str>) {
        let base = Self::for_preset(self.preset);
        let pick = |want_on: bool| {
            all()
                .filter(|c| self.is_on(c) == want_on && base.is_on(c) != want_on)
                .map(|c| c.id)
                .collect()
        };
        (pick(true), pick(false))
    }

    /// `card` may be off only while every tool that reads it is hidden from the model:
    /// otherwise the model is offered `prompt_read` for a card it was never shown.
    /// There is no implicit coupling; the fix is the operator's, in `/settings tools`.
    pub fn check_card(&self, visible: &dyn Fn(&str) -> bool) -> Result<(), String> {
        let card_off = DRIVER.iter().any(|c| c.id == "card" && !self.is_on(c));
        let exposed: Vec<&str> = CARD_TOOLS.iter().copied().filter(|t| visible(t)).collect();
        if card_off && !exposed.is_empty() {
            let why = if self.preset == Preset::Pi && !self.is_on_id("card") {
                "preset `pi` turns the `card` component off, and "
            } else {
                ""
            };
            return Err(format!(
                "{why}component `card` can be off only while {} are hidden from the model, \
                 but {} {} visible; hide them with `/settings tools` (for example \
                 `/settings tools pi`) or keep `card` on",
                CARD_TOOLS.join(" and "),
                exposed.join(" and "),
                if exposed.len() == 1 { "is" } else { "are" },
            ));
        }
        Ok(())
    }

    /// The preset and what was changed from it, for the status line:
    /// `default`, `default, 2 off`, `pi`, `pi, 1 changed`.
    #[must_use]
    pub fn summary(&self) -> String {
        let (on, off) = self.deltas();
        match (self.preset, on.len() + off.len()) {
            (Preset::Default, 0) => "default".to_string(),
            (Preset::Default, _) => format!("default, {} off", off.len()),
            (Preset::Pi, 0) => "pi".to_string(),
            (Preset::Pi, n) => format!("pi, {n} changed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_soul(enabled: &dyn Fn(&Component) -> bool, visible: &dyn Fn(&str) -> bool) -> String {
        super::render_soul(enabled, visible, Preset::Default, false)
    }

    fn soul_parts(
        enabled: &dyn Fn(&Component) -> bool,
        visible: &dyn Fn(&str) -> bool,
    ) -> Vec<(&'static str, String)> {
        super::soul_parts(enabled, visible, Preset::Default, false)
    }

    fn all(_: &str) -> bool {
        true
    }

    /// The soul without the paragraphs that left the default prompt for skills
    /// (`habit-present-findings`, `three-cs`): the default set renders to this.
    fn soul_without_present_findings() -> String {
        let mut text = crate::DEFAULT_SOUL.to_string();
        for marker in ["**Present findings", "**Working code first"] {
            let start = text
                .find(marker)
                .expect("the oracle still has the paragraph");
            let end = text[start..]
                .find("\n\n")
                .map_or(text.len(), |n| start + n + 2);
            text.replace_range(start..end, "");
        }
        text.trim_end_matches('\n').to_string()
    }

    /// #2933: ALL the components, switched on, ARE the oracle soul; the default set is
    /// that soul minus exactly the `habit-present-findings` paragraph.
    #[test]
    fn components_join_to_default_soul() {
        assert_eq!(render_soul(&|_| true, &all), crate::DEFAULT_SOUL);
        assert_eq!(default_soul(&all), soul_without_present_findings());
    }

    #[test]
    fn ids_are_unique_described_and_kebab_case() {
        let mut seen = std::collections::BTreeSet::new();
        for c in super::all() {
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
        assert_eq!(super::all().count(), 38);
    }

    #[test]
    fn every_component_but_the_skills_is_on_by_default() {
        let off: Vec<&str> = super::all()
            .filter(|c| !c.default_on)
            .map(|c| c.id)
            .collect();
        assert_eq!(off, ["habit-present-findings", "three-cs"]);
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
        let one_rule = render_soul(&|c| c.id == "habit-ground-truth", &all);
        assert!(one_rule.starts_with("## How to work\n\n**Seek ground truth"));
    }

    #[test]
    fn a_rule_is_dropped_when_its_tool_is_hidden() {
        let text = default_soul(&|t| t != "render_report");
        assert!(!text.contains("Present findings"));
        assert!(text.contains("Prefer edit_file"));
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parts_join_to_the_soul_and_carry_the_heading_with_the_first_rule() {
        let parts = soul_parts(&|_| true, &all);
        assert_eq!(parts.len(), 13);
        assert_eq!(parts[0].0, "identity");
        assert!(parts[3].1.starts_with("## How to work\n\n**One change"));
        assert_eq!(render_soul(&|_| true, &all), crate::DEFAULT_SOUL);
    }

    /// #2993 review: the listing sizes a component from the selected preset's
    /// rendering. The identity is 75 characters under `default` and 29 under `pi`;
    /// the tools line follows the visible tools under both.
    #[test]
    fn sizes_follow_the_selected_preset() {
        let chars = |set: &ComponentSet, id: &str, visible: &dyn Fn(&str) -> bool| {
            rows(set, visible)
                .into_iter()
                .find(|r| r.id == id)
                .unwrap()
                .chars
        };
        let default = ComponentSet::default();
        let pi = ComponentSet::for_preset(Preset::Pi);
        assert_eq!(IDENTITY.chars().count(), 75);
        assert_eq!(chars(&default, "identity", &all), Some(75));
        assert_eq!(chars(&pi, "identity", &all), Some(29));
        assert_eq!(PI_IDENTITY.chars().count(), 29);
        let four = |t: &str| matches!(t, "read_file" | "write_file" | "edit_file" | "run_command");
        let line = "You have tools: run_command, read_file, write_file, edit_file.";
        assert_eq!(chars(&default, "tools", &four), Some(line.chars().count()));
        assert_eq!(chars(&pi, "tools", &four), Some(line.chars().count()));
        // A text component that pi leaves alone has one size under both.
        assert_eq!(
            chars(&default, "habit-ground-truth", &all),
            chars(&pi, "habit-ground-truth", &all)
        );
    }

    #[test]
    fn rows_report_size_state_and_the_missing_tool() {
        let set = ComponentSet::default()
            .with("habit-ground-truth", false)
            .unwrap();
        let rows = rows(&set, &|t| t != "render_report");
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap().clone();
        assert!(!row("habit-ground-truth").on && row("habit-ground-truth").chars > Some(400));
        assert_eq!(row("habit-present-findings").needs, Some("render_report"));
        assert_eq!(row("habit-present-findings").chars, Some(0));
        assert_eq!(row("identity").needs, None);
    }

    #[test]
    fn deltas_and_summary_round_trip_through_resolve() {
        let set = ComponentSet::default()
            .with("habit-ground-truth", false)
            .unwrap()
            .with("habit-read-minimum", false)
            .unwrap();
        assert_eq!(set.summary(), "default, 2 off");
        assert_eq!(ComponentSet::default().summary(), "default");
        let (on, off) = set.deltas();
        let again = ComponentSet::resolve(
            None,
            &on.iter().map(ToString::to_string).collect::<Vec<_>>(),
            &off.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(again, set);
        assert!(set
            .with("nope", true)
            .unwrap_err()
            .contains("habit-ground-truth"));
    }

    #[test]
    fn no_config_is_the_default_set() {
        let set = ComponentSet::resolve(None, &[], &[]).unwrap();
        assert!(set.is_default());
        assert_eq!(
            render_soul(&|c| set.is_on(c), &all),
            soul_without_present_findings()
        );
    }

    /// #2933 PR3: `off = ["habit-ground-truth"]` removes exactly that paragraph.
    #[test]
    fn off_a_rule_drops_exactly_that_paragraph() {
        let set = ComponentSet::resolve(None, &[], &strings(&["habit-ground-truth"])).unwrap();
        let text = render_soul(&|c| set.is_on(c), &all);
        let base = soul_without_present_findings();
        let start = base.find("**Seek ground truth.**").unwrap();
        let end = start + base[start..].find("\n\n").unwrap() + 2;
        assert_eq!(text, format!("{}{}", &base[..start], &base[end..]));
    }

    #[test]
    fn on_wins_over_the_preset_and_off_composes() {
        let set = ComponentSet::resolve(
            Some("default"),
            &strings(&["identity"]),
            &strings(&["tools"]),
        )
        .unwrap();
        assert!(!set.is_default());
        let text = render_soul(&|c| set.is_on(c), &all);
        assert!(text.starts_with(
            "You are newt, a free, friendly, local agentic coder. Be concise and direct. Follow"
        ));
    }

    #[test]
    fn unknown_ids_and_presets_are_refused_naming_the_valid_ones() {
        let err = ComponentSet::resolve(None, &[], &strings(&["nope"])).unwrap_err();
        assert!(
            err.contains("`nope`") && err.contains("habit-ground-truth"),
            "{err}"
        );
        let err = ComponentSet::resolve(None, &strings(&["nope"]), &[]).unwrap_err();
        assert!(err.contains("`nope`"), "{err}");
        let err = ComponentSet::resolve(Some("fancy"), &[], &[]).unwrap_err();
        assert!(err.contains("presets: default, pi"), "{err}");
        assert!(ComponentSet::resolve(Some("headless"), &[], &[])
            .unwrap_err()
            .contains("headless-only"));
        let both = strings(&["habit-ground-truth"]);
        assert!(ComponentSet::resolve(None, &both, &both)
            .unwrap_err()
            .contains("both"));
    }

    #[test]
    fn the_turn_and_driver_pieces_are_named_with_their_layers() {
        let ids: Vec<(&str, Layer)> = TURN.iter().chain(DRIVER).map(|c| (c.id, c.layer)).collect();
        assert_eq!(
            ids,
            [
                ("workspace-state", Layer::Turn),
                ("environment", Layer::Turn),
                ("runtime-authority", Layer::Turn),
                ("git-identity", Layer::Turn),
                ("fs-confinement", Layer::Turn),
                ("operating-mode", Layer::Turn),
                ("card", Layer::Driver),
            ]
        );
        let fences: Vec<&str> = super::all().filter(|c| c.fence).map(|c| c.id).collect();
        assert_eq!(fences, ["runtime-authority", "fs-confinement"]);
        let rows = rows(&ComponentSet::default(), &all_visible);
        assert!(rows
            .iter()
            .filter(|r| r.layer != Layer::Soul)
            .all(|r| r.chars.is_none()));
    }

    fn all_visible(_: &str) -> bool {
        true
    }

    /// #2933 PR5: `card` is refused off while a tool that reads it is visible.
    #[test]
    fn card_may_be_off_only_while_its_tools_are_hidden() {
        let off = ComponentSet::default().with("card", false).unwrap();
        let err = off.check_card(&all_visible).unwrap_err();
        assert!(
            err.contains("/settings tools") && err.contains("prompt_read and artifact_read"),
            "{err}"
        );
        let one = off.check_card(&|t| t == "artifact_read").unwrap_err();
        assert!(one.contains("artifact_read is visible"), "{one}");
        assert!(off
            .check_card(&|t| t != "prompt_read" && t != "artifact_read")
            .is_ok());
        // On is always fine, and so is any other component off.
        assert!(ComponentSet::default().check_card(&all_visible).is_ok());
        let other = ComponentSet::default()
            .with("fs-confinement", false)
            .unwrap();
        assert!(other.check_card(&all_visible).is_ok());
    }

    /// #2933 PR6: preset `pi` keeps exactly identity, tools, workspace-header,
    /// project-instructions and persona, and says `pi` until something is changed.
    #[test]
    fn preset_pi_keeps_its_components_and_turns_card_off() {
        let pi = ComponentSet::resolve(Some("pi"), &[], &[]).unwrap();
        assert_eq!(pi.preset(), Preset::Pi);
        let on: Vec<&str> = super::all().filter(|c| pi.is_on(c)).map(|c| c.id).collect();
        assert_eq!(
            on,
            [
                "identity",
                "tools",
                "coach-identity",
                "project-instructions",
                "persona",
                "workspace-header"
            ]
        );
        assert!(!pi.is_on_id("card"));
        assert!(!pi.is_default());
        assert_eq!(pi.summary(), "pi");
        assert_eq!(pi.deltas(), (Vec::new(), Vec::new()));
        // Deltas are relative to the preset and round-trip through resolve.
        let tweaked = pi
            .with("readme", true)
            .unwrap()
            .with("persona", false)
            .unwrap();
        assert_eq!(tweaked.summary(), "pi, 2 changed");
        let (on, off) = tweaked.deltas();
        let strings = |v: Vec<&str>| v.into_iter().map(String::from).collect::<Vec<_>>();
        let again = ComponentSet::resolve(Some("pi"), &strings(on), &strings(off)).unwrap();
        assert_eq!(again, tweaked);
    }

    /// The pi soul is the one identity line and the tools line, nothing else.
    #[test]
    fn the_pi_soul_is_one_line_and_the_tool_list() {
        let pi = ComponentSet::for_preset(Preset::Pi);
        let soul = super::render_soul(
            &|c| pi.is_on(c),
            &|t| matches!(t, "read_file" | "run_command"),
            Preset::Pi,
            false,
        );
        assert_eq!(
            soul,
            "You are newt, a coding agent. You have tools: run_command, read_file."
        );
        // Under the default preset the identity is the long one.
        assert!(render_soul(&|c| c.id == "identity", &all).starts_with("You are newt, a free"));
    }

    /// Preset `pi` turns `card` off, so it needs the card's tools hidden, and says why.
    #[test]
    fn preset_pi_is_refused_unless_the_card_tools_are_hidden() {
        let pi = ComponentSet::for_preset(Preset::Pi);
        let err = pi.check_card(&all).unwrap_err();
        assert!(
            err.contains("preset `pi` turns the `card` component off")
                && err.contains("/settings tools pi"),
            "{err}"
        );
        assert!(pi
            .check_card(&|t| t != "prompt_read" && t != "artifact_read")
            .is_ok());
        // Turning the card back on under pi is the explicit pairing-free way out.
        assert!(pi.with("card", true).unwrap().check_card(&all).is_ok());
    }

    #[test]
    fn provider_blocks_map_to_their_components() {
        for (provider, id) in [
            ("agents", "project-instructions"),
            ("note_store", "notes"),
            ("memory_index", "memory-index"),
            ("project_map", "project-map"),
            ("api_surface", "api-surface"),
            ("knowledge_base", "api-surface"),
        ] {
            assert_eq!(provider_component(provider), Some(id));
            assert!(super::all().any(|c| c.id == id));
        }
        assert_eq!(provider_component("soul"), None);
    }

    /// #2933 PR7: the coach altitude's components are the coach oracle, all on.
    #[test]
    fn coach_components_join_to_coach_soul() {
        let text = super::render_soul(&|_| true, &all, Preset::Default, true);
        assert_eq!(text, crate::COACH_SOUL);
        let parts = super::soul_parts(&|_| true, &all, Preset::Default, true);
        let ids: Vec<&str> = parts.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "coach-identity",
                "coach-advise",
                "coach-ground",
                "coach-show-command",
                "coach-teach",
                "coach-stop-unsure"
            ]
        );
        // The heading rides with the first rule, as the doer's does.
        assert!(parts[1].1.starts_with("## How to coach\n\n**Advise;"));
        // The two altitudes never mix.
        let doer = super::render_soul(&|_| true, &all, Preset::Default, false);
        assert!(!doer.contains("COACH mode") && !text.contains("agentic coder"));
    }

    /// Under pi a coach persona still gets its one identity paragraph.
    #[test]
    fn pi_keeps_only_the_coach_identity_at_coach_altitude() {
        let pi = ComponentSet::for_preset(Preset::Pi);
        let text = super::render_soul(&|c| pi.is_on(c), &all, Preset::Pi, true);
        assert!(text.starts_with("You are newt in COACH mode"), "{text}");
        assert!(!text.contains("## How to coach"));
    }
}
