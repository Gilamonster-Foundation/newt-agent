//! Deterministic prompt-comprehension intake for a single operator turn.
//!
//! The intake runs after a durable prompt receipt exists and before the model,
//! tool catalog, or action nudges run. It makes a small, inspectable decision:
//! whether the turn is an `ask`, `act`, `explain`, `research`, or harness-
//! selected `plan` turn; which atomic asks it contains; and whether an operator
//! decision remains unlocked.
//!
//! This is deliberately a bounded heuristic, not an LLM judge. The security
//! contract is fail-closed at the dispatcher: classification changes what is
//! advertised and what can run, while a fabricated tool call is still checked
//! against the resulting disposition at execution time.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Fixed, content-free marker appended inside the protected active-prompt
/// card. Keep this stable: `prompt_read` uses it to recognize only harness
/// owned augmented cards during replacement.
pub(crate) const PROMPT_COMPREHENSION_MODEL_CARD_PREFIX: &str = "[NEWT PROMPT COMPREHENSION v1]";

const MAX_ATOMIC_ASKS: usize = 64;
const MAX_DECISIONS: usize = 16;
// Reserve one slot for an overflow lock. Silently omitting the Nth unresolved
// decision would let a reply to the first N-1 decisions unlock execution.
const MAX_CONCRETE_DECISIONS: usize = MAX_DECISIONS - 1;
const MAX_ASK_BYTES: usize = 4_096;
const MAX_CLARIFICATION_BYTES: usize = 384;
/// Adjudication is one bounded side call, never a second agent turn. A batch
/// larger than this is refused outright and every candidate stays `Pending`.
pub const MAX_ADJUDICATION_BATCH: usize = 15;
const RESEARCH_TOOL_ROUND_LIMIT: usize = 3;
pub(super) const PROMPT_COMPREHENSION_SCHEMA_V1: &str = "prompt_comprehension_manifest_v1";
pub(super) const PROMPT_COMPREHENSION_SCHEMA_V2: &str = "prompt_comprehension_manifest_v2";
/// #1971: adds `atomic_ask_kinds` and, for informational clauses only, their
/// text. A v3 reader accepts v1/v2 records unchanged — the new fields are
/// optional, and an absent list is exactly equivalent to an empty one.
pub(super) const PROMPT_COMPREHENSION_SCHEMA_V3: &str = "prompt_comprehension_manifest_v3";
pub(super) const PROMPT_COMPREHENSION_SCHEMA_CURRENT: &str = PROMPT_COMPREHENSION_SCHEMA_V3;

/// Where the live [`PromptDisposition`] came from, so the model card can say
/// so truthfully (#2051 review).
///
/// The card's provenance clause once said, unconditionally, that the harness
/// inferred the disposition from the operator's words and the operator did not
/// choose it. That is false under an explicit `/mode plan` or `/mode diagnose`,
/// which reach [`PromptIntake::enforce_read_only`] and narrow the turn on the
/// operator's own standing instruction. The two arms name the two callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispositionSource {
    /// Classified from the prompt text by the intake lexicon.
    Inferred,
    /// Narrowed after classification by a session setting the operator chose
    /// earlier (an operating mode); the prompt's own words did not decide it.
    SessionPolicy,
}

/// The harness-selected mode for one accepted prompt receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDisposition {
    /// A decision is not locked; the harness asks a bounded batch and ends the
    /// turn before model inference.
    Ask,
    /// The task is ready for normal execution.
    Act,
    /// Answer or clarify with no mutations; only bounded reads are available.
    Explain,
    /// Gather bounded read-only evidence; mutations and capability grants are
    /// unavailable.
    Research,
    /// Read/recover and update only the harness-owned plan ledger; workspace,
    /// execution, network, capability-grant, and generic MCP mutation paths
    /// remain unavailable.
    Plan,
}

impl PromptDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Act => "act",
            Self::Explain => "explain",
            Self::Research => "research",
            Self::Plan => "plan",
        }
    }

    /// Bound non-execution turns even when the session config allows a large
    /// tool loop. `Ask` is terminal in the TUI and gets a zero-round defense in
    /// depth for headless callers.
    pub fn tool_round_limit(self, max: usize) -> usize {
        match self {
            Self::Ask => 0,
            Self::Act => max,
            Self::Explain => max,
            Self::Research => max.min(RESEARCH_TOOL_ROUND_LIMIT),
            Self::Plan => max,
        }
    }
}

/// What one extracted clause *is* — an instruction to act on, or a fact the
/// operator stated (#1971).
///
/// Splitting clauses without asking this question is how a pure FYI became one
/// atomic ask with `disposition=act` and the full round budget: the intake had
/// no vocabulary for "the operator told me something" as distinct from "the
/// operator asked me to do something".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// Something to do. The historical and still-default reading.
    Instruction,
    /// Something stated. Carries content the turn must not lose, and carries
    /// **no authorization**.
    Informational,
}

impl AskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instruction => "instruction",
            Self::Informational => "informational",
        }
    }
}

/// One bounded clause extracted from a monolithic prompt. Text remains only in
/// memory, in the durable prompt receipt, and — for an informational clause
/// only — in the durable artifact; see [`PromptIntake::artifact_metadata`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicAsk {
    text: String,
    kind: AskKind,
}

impl AtomicAsk {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether this clause instructs or merely states.
    pub fn kind(&self) -> AskKind {
        self.kind
    }

    pub fn is_informational(&self) -> bool {
        self.kind == AskKind::Informational
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionStatus {
    Pending,
    Locked,
}

impl DecisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Locked => "locked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    Operator,
    Policy,
    AuthorizedAssumption,
}

impl DecisionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Policy => "policy",
            Self::AuthorizedAssumption => "authorized_assumption",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecisionLock {
    question: String,
    status: DecisionStatus,
    source: Option<DecisionSource>,
    /// The explicit interpretation the agent will proceed under. Present iff
    /// the lock source is [`DecisionSource::AuthorizedAssumption`]: an
    /// assumption may never be silently inferred, so a lock without stated
    /// text is invalid and [`PromptIntake::validate`] rejects it.
    assumption: Option<String>,
    /// An intake-bound overflow is not a decision the operator can answer in
    /// place. The operator must split the request before execution can resume.
    overflow: bool,
}

impl DecisionLock {
    pub fn question(&self) -> &str {
        &self.question
    }

    pub fn status(&self) -> DecisionStatus {
        self.status
    }

    pub fn source(&self) -> Option<DecisionSource> {
        self.source
    }

    pub fn is_overflow(&self) -> bool {
        self.overflow
    }

    /// The recorded assumption for an [`DecisionSource::AuthorizedAssumption`]
    /// lock. `None` for pending decisions and for operator answers — auditing
    /// must be able to tell a stated operator answer from a model-adjudicated
    /// assumption, so the two never share a representation.
    pub fn assumption(&self) -> Option<&str> {
        self.assumption.as_deref()
    }
}

/// One candidate handed to the adjudicator. `id` is the 1-based ordinal the
/// model must echo back; it indexes the CANDIDATE list, never the decision
/// vector, so a model reply can never address a decision that was not offered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdjudicationCandidate {
    pub id: usize,
    pub question: String,
}

/// One adjudicated verdict. This is the entire authority the model has: it may
/// say "the operator delegated this, and here is the interpretation I will
/// proceed under". It cannot lock, cannot unlock, and cannot name a decision
/// that was not in the candidate batch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AdjudicationVerdict {
    pub decision_id: usize,
    #[serde(default)]
    pub delegated_to_agent: bool,
    #[serde(default)]
    pub assumption: String,
}

/// Why an adjudication batch was refused wholesale. Fail-closed: the candidates
/// stay `Pending` and the operator is asked.
///
/// A side call that errors, times out, or returns malformed output is NOT
/// represented here — that is handled where the call is made, by returning the
/// intake untouched. This type is only for a batch the harness refuses to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdjudicationRefusal {
    /// More candidates than one bounded side call may adjudicate.
    BatchTooLarge { candidates: usize, bound: usize },
}

/// Content-bearing in-memory comprehension result. Its public accessors expose
/// counts and bounded asks for UI rendering; [`PromptIntake::artifact_metadata`]
/// is the text-free persistence projection.
#[derive(Debug, Clone)]
pub struct PromptComprehensionManifest {
    atomic_asks: Vec<AtomicAsk>,
    decisions: Vec<DecisionLock>,
}

impl PromptComprehensionManifest {
    pub fn atomic_asks(&self) -> &[AtomicAsk] {
        &self.atomic_asks
    }

    /// The clauses that STATE rather than ask (#1971).
    pub fn informational_asks(&self) -> impl Iterator<Item = &AtomicAsk> {
        self.atomic_asks.iter().filter(|a| a.is_informational())
    }

    pub fn decision_count(&self) -> usize {
        self.decisions.len()
    }

    pub fn decisions(&self) -> &[DecisionLock] {
        &self.decisions
    }

    pub fn pending_decision_count(&self) -> usize {
        self.decisions
            .iter()
            .filter(|decision| decision.status == DecisionStatus::Pending)
            .count()
    }

    pub fn locked_decision_count(&self) -> usize {
        self.decisions
            .iter()
            .filter(|decision| decision.status == DecisionStatus::Locked)
            .count()
    }
}

/// The validated intake passed through the harness for one turn.
#[derive(Debug, Clone)]
pub struct PromptIntake {
    manifest: PromptComprehensionManifest,
    disposition: PromptDisposition,
    /// The disposition selected from the task itself. An unresolved decision
    /// temporarily changes the live disposition to `Ask`; an explicit answer
    /// restores this value once every decision is locked.
    post_lock_disposition: PromptDisposition,
    /// Whether the live disposition is the lexicon's reading of the prompt or
    /// a session policy's narrowing of it. Read by the model card only.
    source: DispositionSource,
    /// Why the LAST clarification reply failed to lock the batch, if it did.
    ///
    /// #1689 item 1. Set only by [`Self::resolve_with_operator_answer`]; a
    /// freshly analyzed intake has none. The harness reads it to say what was
    /// wrong instead of re-emitting the identical block, which is the whole
    /// difference between a blocked session and one that looks hung.
    last_rejection: Option<ClarificationRejection>,
}

impl PromptIntake {
    /// Analyze a new operator prompt before any model-visible work begins,
    /// classifying its disposition against `lexicon` (#1260 — the operator's
    /// `[intake]` overrides; [`DispositionLexicon::default`] = built-ins).
    pub fn analyze_with(prompt: &str, lexicon: &DispositionLexicon) -> Self {
        let mut intake = Self::analyze(prompt);
        if prompt.trim().is_empty() {
            return intake; // the empty-prompt Ask terminal is not lexicon-driven
        }
        // Re-derive the lexicon-driven part. Ask KINDS are lexicon-driven too
        // (#1971), so the asks are re-extracted rather than reused; decisions
        // depend only on ask text, which the re-extraction reproduces.
        let (asks, _) = extract_atomic_asks_with(prompt, lexicon);
        intake.post_lock_disposition = infer_disposition_with(prompt, &asks, lexicon);
        intake.manifest.atomic_asks = asks;
        if intake.disposition != PromptDisposition::Ask {
            intake.disposition = intake.post_lock_disposition;
        }
        debug_assert!(intake.validate().is_ok());
        intake
    }

    /// Analyze a new operator prompt before any model-visible work begins.
    pub fn analyze(prompt: &str) -> Self {
        if prompt.trim().is_empty() {
            let intake = Self {
                manifest: PromptComprehensionManifest {
                    atomic_asks: vec![AtomicAsk {
                        text: "(empty operator prompt)".to_string(),
                        kind: AskKind::Instruction,
                    }],
                    decisions: vec![DecisionLock {
                        question: "Provide a non-empty task before execution.".to_string(),
                        status: DecisionStatus::Pending,
                        source: None,
                        assumption: None,
                        overflow: false,
                    }],
                },
                disposition: PromptDisposition::Ask,
                post_lock_disposition: PromptDisposition::Explain,
                source: DispositionSource::Inferred,
                last_rejection: None,
            };
            debug_assert!(intake.validate().is_ok());
            return intake;
        }
        let (atomic_asks, atomic_overflow) = extract_atomic_asks(prompt);
        let post_lock_disposition = infer_disposition(prompt, &atomic_asks);
        let (mut decisions, decision_overflow) = extract_decisions(&atomic_asks);
        if atomic_overflow || decision_overflow {
            decisions.push(overflow_decision());
        }
        let disposition = if decisions
            .iter()
            .any(|decision| decision.status == DecisionStatus::Pending)
        {
            PromptDisposition::Ask
        } else {
            post_lock_disposition
        };
        let intake = Self {
            manifest: PromptComprehensionManifest {
                atomic_asks,
                decisions,
            },
            disposition,
            post_lock_disposition,
            source: DispositionSource::Inferred,
            last_rejection: None,
        };
        debug_assert!(intake.validate().is_ok());
        intake
    }

    pub fn disposition(&self) -> PromptDisposition {
        self.disposition
    }

    /// Select an explicit non-action disposition for this accepted intake.
    ///
    /// Operating modes are applied after deterministic prompt intake. They may
    /// choose `Explain`, `Research`, or `Plan`, but must never turn a pending
    /// `Ask` into executable work or select `Act`. Updating both live and
    /// post-lock state keeps the model card, durable artifact, advertised
    /// catalog, and dispatcher on one effective disposition.
    pub fn enforce_read_only(&mut self, disposition: PromptDisposition) {
        if !matches!(
            disposition,
            PromptDisposition::Explain | PromptDisposition::Research | PromptDisposition::Plan
        ) {
            debug_assert!(false, "read-only disposition required");
            return;
        }
        if self.disposition != PromptDisposition::Ask {
            self.disposition = disposition;
            self.post_lock_disposition = disposition;
            // The narrowing came from a session setting, not the prompt; the
            // card must not claim the operator had no hand in it.
            self.source = DispositionSource::SessionPolicy;
        }
        debug_assert!(self.validate().is_ok());
    }

    /// Whether the live disposition was inferred from the prompt or narrowed
    /// by a session policy the operator set.
    pub fn disposition_source(&self) -> DispositionSource {
        self.source
    }

    pub fn atomic_asks(&self) -> &[AtomicAsk] {
        self.manifest.atomic_asks()
    }

    pub fn manifest(&self) -> &PromptComprehensionManifest {
        &self.manifest
    }

    /// Render the complete bounded clarification batch for the operator. This
    /// content intentionally never enters model/system context or artifact
    /// metadata; it is presented directly by the harness and the turn ends.
    pub fn clarification_batch(&self) -> String {
        let pending = self.pending_indices();
        if pending.is_empty() {
            return String::new();
        }

        // #1689 item 3: singular phrasing for a single item. "these decisions"
        // and "every item" over a one-item list is what made a COMPLETE batch
        // read as a truncated one, and sent an operator hunting for output that
        // had never been withheld.
        let mut rendered = String::from(if pending.len() == 1 {
            "I need this decision locked before I can execute. Reply with an explicit ordinal, for example `1: …`:\n"
        } else {
            "I need these decisions locked before I can execute. Reply using an explicit ordinal for every item, for example `1: …`:\n"
        });
        // #1689 item 4: ordinals come from the SAME mapping the resolver reads
        // (`pending_indices`), so a displayed "3." is always the "3:" that
        // resolves. The old code enumerated BEFORE filtering, numbering by
        // absolute position in `decisions` while the resolver indexed into the
        // pending-only slice. Unreachable today only because locking is
        // all-or-nothing; the moment anything locks a decision on its own — the
        // policy resolver the comment below anticipates — the two diverge, and
        // the gate starts displaying "3." while accepting only "1:".
        for (ordinal, index) in pending.iter().enumerate() {
            let decision = &self.manifest.decisions[*index];
            let question = truncate_chars(&decision.question, MAX_CLARIFICATION_BYTES);
            rendered.push_str(&format!("{}. {}\n", ordinal + 1, question));
        }
        rendered.trim_end().to_string()
    }

    /// Why the last clarification reply was refused, if it was (#1689 item 1).
    ///
    /// `None` on a freshly analyzed intake and after a reply that locked the
    /// batch. The harness prints [`ClarificationRejection::explain`] above the
    /// re-emitted batch so a second identical block is never the whole
    /// response to a rejected answer.
    pub fn last_rejection(&self) -> Option<&ClarificationRejection> {
        self.last_rejection.as_ref()
    }

    /// The absolute `decisions` indices that are still pending, in order.
    ///
    /// #1689 item 4. This is the ONE mapping between a displayed ordinal and a
    /// decision: position `n` here is the ordinal `n + 1` the operator types,
    /// for both [`Self::clarification_batch`] and `explicit_answer_indices`.
    /// Keeping render and resolve on one function is what makes them unable to
    /// disagree, rather than merely observed to agree.
    fn pending_indices(&self) -> Vec<usize> {
        self.manifest
            .decisions
            .iter()
            .enumerate()
            .filter_map(|(index, decision)| {
                (decision.status == DecisionStatus::Pending).then_some(index)
            })
            .collect()
    }

    /// Resolve only explicit operator answers against this pending manifest.
    ///
    /// Every pending decision requires an explicit ordinal (`1: …`, `2: …`).
    /// This deliberately rejects acknowledgements such as `continue`: an LLM
    /// must never infer a decision value merely because the operator resumed
    /// the conversation.
    pub fn resolve_with_operator_answer(&self, answer: &str) -> Self {
        // An empty first receipt has no task semantics to preserve. Its direct
        // clarification answer is therefore a new operator task under the
        // same prompt root and must receive a fresh, fail-closed intake.
        if self
            .manifest
            .atomic_asks
            .first()
            .is_some_and(|ask| ask.text == "(empty operator prompt)")
        {
            return Self::analyze(answer);
        }
        // Overflow means the original request exceeded the bounded intake
        // representation. No answer can safely prove every omitted ask is
        // resolved, so retain Ask until the operator starts a smaller task.
        if self
            .manifest
            .decisions
            .iter()
            .any(DecisionLock::is_overflow)
        {
            return self.clone();
        }
        let mut resolved = self.clone();
        // #1689 item 4: the same mapping `clarification_batch` renders from.
        let pending = resolved.pending_indices();

        match explicit_answer_outcome(answer, &pending) {
            Ok(indices) => {
                for index in indices {
                    let decision = &mut resolved.manifest.decisions[index];
                    decision.status = DecisionStatus::Locked;
                    decision.source = Some(DecisionSource::Operator);
                }
                resolved.last_rejection = None;
            }
            // #1689 item 1: carry WHY, so the harness can say something other
            // than the identical block a second time.
            Err(rejection) => resolved.last_rejection = Some(rejection),
        }

        resolved.disposition = if resolved.manifest.pending_decision_count() == 0 {
            resolved.post_lock_disposition
        } else {
            PromptDisposition::Ask
        };
        debug_assert!(resolved.validate().is_ok());
        resolved
    }

    /// The pending, non-overflow decisions offered to the adjudicator, in
    /// order. The heuristic alone decides what appears here; adjudication is
    /// strictly a filter over this list (#1749).
    pub fn adjudication_candidates(&self) -> Vec<AdjudicationCandidate> {
        self.candidate_indices()
            .into_iter()
            .enumerate()
            .map(|(ordinal, index)| AdjudicationCandidate {
                id: ordinal + 1,
                question: self.manifest.decisions[index].question.clone(),
            })
            .collect()
    }

    /// Absolute `decisions` indices eligible for adjudication.
    fn candidate_indices(&self) -> Vec<usize> {
        self.manifest
            .decisions
            .iter()
            .enumerate()
            .filter_map(|(index, decision)| {
                (decision.status == DecisionStatus::Pending && !decision.overflow).then_some(index)
            })
            .collect()
    }

    /// Apply model verdicts under harness-owned rules. The model proposes; this
    /// function is the only thing that may move state, and the ONLY transition
    /// it can perform is `Pending -> Locked(AuthorizedAssumption)`:
    ///
    /// * a `decision_id` outside the candidate batch is discarded;
    /// * `delegated_to_agent: false` leaves the decision pending;
    /// * an empty/whitespace assumption is refused — an assumption is never
    ///   silently inferred (#1749);
    /// * an already-locked decision is never touched, so a model cannot
    ///   overwrite or re-source an operator answer;
    /// * a duplicate `decision_id` after the first is discarded.
    ///
    /// An oversized batch refuses wholesale rather than adjudicating a prefix.
    pub fn apply_adjudications(
        &self,
        verdicts: &[AdjudicationVerdict],
    ) -> Result<Self, AdjudicationRefusal> {
        let candidates = self.candidate_indices();
        if candidates.len() > MAX_ADJUDICATION_BATCH {
            return Err(AdjudicationRefusal::BatchTooLarge {
                candidates: candidates.len(),
                bound: MAX_ADJUDICATION_BATCH,
            });
        }

        let mut resolved = self.clone();
        let mut seen = BTreeSet::new();
        for verdict in verdicts {
            if !verdict.delegated_to_agent {
                continue;
            }
            let assumption = verdict.assumption.trim();
            if assumption.is_empty() {
                continue;
            }
            let Some(index) = verdict
                .decision_id
                .checked_sub(1)
                .and_then(|ordinal| candidates.get(ordinal).copied())
            else {
                continue;
            };
            if !seen.insert(index) {
                continue;
            }
            let decision = &mut resolved.manifest.decisions[index];
            debug_assert_eq!(decision.status, DecisionStatus::Pending);
            decision.status = DecisionStatus::Locked;
            decision.source = Some(DecisionSource::AuthorizedAssumption);
            decision.assumption = Some(
                truncate_chars(assumption, MAX_CLARIFICATION_BYTES)
                    .trim()
                    .to_string(),
            );
        }

        resolved.disposition = if resolved.manifest.pending_decision_count() == 0 {
            resolved.post_lock_disposition
        } else {
            PromptDisposition::Ask
        };
        debug_assert!(resolved.validate().is_ok());
        Ok(resolved)
    }

    /// Absolute indices of model-authorized locks, in order. Position `n` here
    /// is the ordinal `n + 1` the operator types to `/undo-lock`. It is its own
    /// numbering — deliberately NOT shared with the clarification ordinals,
    /// which the resolver reads (#1689 item 4).
    fn assumption_indices(&self) -> Vec<usize> {
        self.manifest
            .decisions
            .iter()
            .enumerate()
            .filter_map(|(index, decision)| {
                (decision.source == Some(DecisionSource::AuthorizedAssumption)).then_some(index)
            })
            .collect()
    }

    /// One operator-facing line per model-authorized assumption. An assumption
    /// the operator never sees is indistinguishable from a silent guess, so
    /// every lock states its interpretation and how to reopen it.
    pub fn authorized_assumption_notices(&self) -> Vec<String> {
        self.assumption_indices()
            .into_iter()
            .enumerate()
            .filter_map(|(ordinal, index)| {
                let assumption = self.manifest.decisions[index].assumption.as_deref()?;
                Some(format!(
                    "Assuming: {assumption} — `/undo-lock {}` to reopen",
                    ordinal + 1
                ))
            })
            .collect()
    }

    /// Reopen a model-authorized assumption by its `/undo-lock` ordinal.
    /// Operator answers are NOT reversible this way: `/undo-lock` exists to
    /// undo the harness's own inference, not to discard what the operator said.
    pub fn undo_lock(&self, ordinal: usize) -> Option<Self> {
        let index = *self.assumption_indices().get(ordinal.checked_sub(1)?)?;
        let mut reopened = self.clone();
        let decision = &mut reopened.manifest.decisions[index];
        decision.status = DecisionStatus::Pending;
        decision.source = None;
        decision.assumption = None;
        reopened.disposition = PromptDisposition::Ask;
        debug_assert!(reopened.validate().is_ok());
        Some(reopened)
    }

    /// Test-only: append `extra` pending candidates beyond what the parser can
    /// produce. `MAX_CONCRETE_DECISIONS` already caps `analyze` at
    /// `MAX_ADJUDICATION_BATCH`, so the adjudication bound is defense in depth
    /// against a future parser bound raise — unreachable through parsing, and
    /// therefore only provable through a direct constructor.
    #[cfg(test)]
    pub(super) fn with_extra_pending_candidates(&self, extra: usize) -> Self {
        let mut widened = self.clone();
        for index in 0..extra {
            widened.manifest.decisions.push(DecisionLock {
                question: format!("Choose the smallest fix for extra-module-{index}."),
                status: DecisionStatus::Pending,
                source: None,
                assumption: None,
                overflow: false,
            });
        }
        widened.disposition = PromptDisposition::Ask;
        widened
    }

    /// Count of locks whose authority is model adjudication rather than an
    /// operator answer. Auditing must be able to tell the two apart.
    pub fn authorized_assumption_count(&self) -> usize {
        self.assumption_indices().len()
    }

    /// Content-free model projection placed inside the protected active-prompt
    /// card. It contains no prompt, decision, or clarification text.
    pub fn model_card(&self) -> String {
        let pending = self.manifest.pending_decision_count();
        let locked = self.manifest.locked_decision_count();
        // #2051: one owner for every sentence the harness says about the
        // disposition. The block carries the action line plus the provenance
        // and privacy clauses — without them a small model reads the action
        // line as an operator-imposed rule and reports its compliance.
        let instruction =
            super::DispositionVoices::default().card_block(self.disposition, self.source);
        let prompt = self
            .manifest
            .atomic_asks
            .iter()
            .map(AtomicAsk::text)
            .collect::<Vec<_>>()
            .join("\n");
        let refinement = request_refinement_model_card(&prompt);
        let mut card = format!(
            "{PROMPT_COMPREHENSION_MODEL_CARD_PREFIX}\n\
             disposition: {}\n\
             atomic_ask_count: {}\n\
             decision_count: {}\n\
             pending_decision_count: {pending}\n\
             locked_decision_count: {locked}\n\
             {instruction}",
            self.disposition.as_str(),
            self.manifest.atomic_asks.len(),
            self.manifest.decisions.len(),
        );
        if !refinement.is_empty() {
            card.push('\n');
            card.push_str(&refinement);
        }
        // #1971: the stated facts, VERBATIM and named as facts.
        //
        // This is the one place the card is not content-free, and the exception
        // is the whole point of the fix. A clause the harness has decided
        // carries no authorization is exactly the clause most likely to be
        // dropped — the evidenced session turned a 92-byte statement into
        // `atomic_ask_count: 1` and nothing else, and the fact it stated was
        // never acted on or mentioned again. A count cannot be read back; the
        // text can.
        let noted: Vec<&str> = self
            .manifest
            .informational_asks()
            .map(AtomicAsk::text)
            .collect();
        if !noted.is_empty() {
            card.push_str(
                "\nnoted_facts: the operator STATED the following; they carry no request to act\n",
            );
            for fact in noted {
                card.push_str("  - ");
                card.push_str(fact);
                card.push('\n');
            }
            card.push_str(
                "noted_instruction: acknowledge these in your reply and carry them forward; \
                 do not treat them as authorization to mutate anything",
            );
        }
        card
    }

    /// Exact persistence projection for a bodyless `Decision` artifact. The
    /// values are scalar counts or BLAKE3 digests; raw prompt-derived text is
    /// intentionally absent.
    pub fn artifact_metadata(&self) -> Value {
        let mut status_counts = serde_json::Map::new();
        status_counts.insert(
            DecisionStatus::Pending.as_str().to_string(),
            Value::from(self.manifest.pending_decision_count() as u64),
        );
        status_counts.insert(
            DecisionStatus::Locked.as_str().to_string(),
            Value::from(self.manifest.locked_decision_count() as u64),
        );

        let mut source_counts = serde_json::Map::new();
        for source in [
            DecisionSource::Operator,
            DecisionSource::Policy,
            DecisionSource::AuthorizedAssumption,
        ] {
            let count = self
                .manifest
                .decisions
                .iter()
                .filter(|decision| decision.source == Some(source))
                .count();
            source_counts.insert(source.as_str().to_string(), Value::from(count as u64));
        }

        json!({
            "schema": PROMPT_COMPREHENSION_SCHEMA_CURRENT,
            "disposition": self.disposition.as_str(),
            "atomic_ask_count": self.manifest.atomic_asks.len() as u64,
            "clarification_count": self.manifest.pending_decision_count() as u64,
            "decision_count": self.manifest.decisions.len() as u64,
            "decision_status_counts": Value::Object(status_counts),
            "decision_source_counts": Value::Object(source_counts),
            "informational_ask_count": self.manifest.informational_asks().count() as u64,
            // #1971. Every other field here is a count or a digest, and that
            // rule is kept for everything an operator DECIDED. Stated facts are
            // the deliberate exception, for three reasons:
            //
            // * a digest cannot be read back, so the durable record of a
            //   dropped fact could not say what was dropped — the artifact that
            //   evidenced this bug recorded `atomic_ask_count=1, bytes=92` and
            //   the fact itself was unrecoverable;
            // * this duplicates nothing: `prompt_receipts.raw_text` already
            //   persists the entire prompt verbatim in the same database, so
            //   the content-free rule was never a privacy boundary here;
            // * it is bounded — informational clauses only, already truncated
            //   to MAX_ASK_BYTES by extraction.
            "informational_asks": self
                .manifest
                .informational_asks()
                .map(|ask| Value::from(ask.text()))
                .collect::<Vec<_>>(),
            "atomic_ask_kinds": self
                .manifest
                .atomic_asks
                .iter()
                .map(|ask| Value::from(ask.kind().as_str()))
                .collect::<Vec<_>>(),
            "atomic_ask_digests": self
                .manifest
                .atomic_asks
                .iter()
                .map(|ask| digest_metadata(&ask.text))
                .collect::<Vec<_>>(),
            "clarification_digests": self
                .manifest
                .decisions
                .iter()
                .filter(|decision| decision.status == DecisionStatus::Pending)
                .map(|decision| digest_metadata(&decision.question))
                .collect::<Vec<_>>(),
            "authorized_assumption_digests": self
                .manifest
                .decisions
                .iter()
                .filter_map(|decision| decision.assumption.as_deref())
                .map(digest_metadata)
                .collect::<Vec<_>>(),
        })
    }

    /// Validate the bounded invariants relied upon by the model-card and
    /// artifact writers. This is public for tests and alternate harnesses.
    pub fn validate(&self) -> Result<(), String> {
        if self.manifest.atomic_asks.len() > MAX_ATOMIC_ASKS {
            return Err("atomic ask count exceeds the intake bound".to_string());
        }
        if self.manifest.decisions.len() > MAX_DECISIONS {
            return Err("decision count exceeds the intake bound".to_string());
        }
        if self.post_lock_disposition == PromptDisposition::Ask {
            return Err("post-lock disposition cannot be ask".to_string());
        }
        for ask in &self.manifest.atomic_asks {
            if ask.text.is_empty() || ask.text.len() > MAX_ASK_BYTES {
                return Err("atomic ask is empty or exceeds the intake bound".to_string());
            }
        }
        for decision in &self.manifest.decisions {
            if decision.question.is_empty() || decision.question.len() > MAX_ASK_BYTES {
                return Err("decision text is empty or exceeds the intake bound".to_string());
            }
            match (decision.status, decision.source) {
                (DecisionStatus::Pending, None) | (DecisionStatus::Locked, Some(_)) => {}
                (DecisionStatus::Pending, Some(_)) => {
                    return Err("pending decision has a lock source".to_string())
                }
                (DecisionStatus::Locked, None) => {
                    return Err("locked decision lacks a source".to_string())
                }
            }
            match (decision.source, decision.assumption.as_deref()) {
                (Some(DecisionSource::AuthorizedAssumption), None | Some("")) => {
                    return Err("authorized assumption lock lacks a stated assumption".to_string())
                }
                (Some(DecisionSource::AuthorizedAssumption), Some(_)) => {}
                (_, Some(_)) => {
                    return Err(
                        "only an authorized assumption may carry assumption text".to_string()
                    )
                }
                (_, None) => {}
            }
        }
        let pending = self.manifest.pending_decision_count();
        if (pending > 0) != (self.disposition == PromptDisposition::Ask) {
            return Err("disposition does not match pending decisions".to_string());
        }
        if pending == 0 && self.disposition != self.post_lock_disposition {
            return Err("resolved disposition does not match task disposition".to_string());
        }
        Ok(())
    }
}

/// Classify one clause as an instruction or a stated fact (#1971).
///
/// # Why this only ever *narrows*
///
/// Measured before it was written: of 22 ordinary operator imperatives — "add
/// a test for the parser", "update the docs", "rebase onto main", "continue",
/// "land it" — **22 reach `Act` solely through the no-match fallback**,
/// because the action lexicon is 17 needles wide. Inverting that fallback, or
/// treating "no recognised imperative" as informational, would silently strip
/// authorization from every one of them.
///
/// So this fires only on POSITIVE evidence that a clause states rather than
/// asks, in two shapes, both pure data:
///
/// 1. an explicit marker at the clause's start (`fyi`, `note that`,
///    `i'll want`) — self-announcing, no further evidence needed;
/// 2. a declarative subject lead (`the`, `it`, `we`, `there`) **and** a
///    stative marker (` is `, ` are `, `'s `) — the "X is now at Y" shape the
///    issue names. Both halves are required: `the` alone would swallow "the
///    tests need updating", and ` is ` alone would swallow "make sure it is
///    green".
///
/// Everything else stays an instruction, which keeps today's behaviour for
/// every clause this cannot positively read. The failure mode of a gap here is
/// therefore an FYI that is still treated as an instruction — the status quo —
/// never an instruction demoted by a lexicon miss.
fn classify_clause(clause: &str, lexicon: &DispositionLexicon) -> AskKind {
    let lower = clause.trim().to_ascii_lowercase();
    let opens_with = |needles: &[String]| {
        needles
            .iter()
            .any(|n| !n.is_empty() && lower.starts_with(n.as_str()))
    };
    if opens_with(&lexicon.informational_markers) {
        return AskKind::Informational;
    }
    // A stative marker is padded on both sides so it matches as a WORD: bare
    // "is" would fire inside "revise", and " is" alone inside "this ".
    let padded = format!(" {lower} ");
    let stative = lexicon
        .stative_markers
        .iter()
        .any(|m| !m.is_empty() && padded.contains(m.as_str()));
    if stative && opens_with(&lexicon.declarative_leads) {
        return AskKind::Informational;
    }
    AskKind::Instruction
}

fn extract_atomic_asks(prompt: &str) -> (Vec<AtomicAsk>, bool) {
    extract_atomic_asks_with(prompt, &DispositionLexicon::default())
}

fn extract_atomic_asks_with(prompt: &str, lexicon: &DispositionLexicon) -> (Vec<AtomicAsk>, bool) {
    let mut asks = Vec::new();
    for line in prompt.lines() {
        let line = strip_list_marker(line.trim());
        if line.is_empty() {
            continue;
        }
        for semicolon_clause in line.split(';') {
            // Treat a period followed by a space as a bounded sentence
            // separator too. This is intentionally shallow rather than a
            // natural-language parser, but it covers the common monolithic
            // "choose X. then choose Y" prompt shape without splitting URLs.
            for clause in semicolon_clause.split(". ") {
                let clause = clause.trim();
                if clause.is_empty() {
                    continue;
                }
                if asks.len() == MAX_ATOMIC_ASKS {
                    return (asks, true);
                }
                asks.push(AtomicAsk {
                    text: truncate_chars(clause, MAX_ASK_BYTES),
                    kind: classify_clause(clause, lexicon),
                });
            }
        }
    }
    if asks.is_empty() {
        asks.push(AtomicAsk {
            text: "(empty operator prompt)".to_string(),
            kind: AskKind::Instruction,
        });
    }
    (asks, false)
}

fn strip_list_marker(line: &str) -> &str {
    let line = line.trim_start_matches(['-', '*', '•', ' ']);
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(['.', ')']) {
            return rest.trim_start();
        }
    }
    line
}

/// Prompt-specific refinements layered over the standing response/repository
/// policy in the protected active-prompt card. Keep this narrow: general
/// Markdown shape and source-first evidence belong to the harness policy, not
/// an incident-derived prompt lexicon.
fn request_refinement_model_card(prompt: &str) -> String {
    let lower = prompt.to_ascii_lowercase();
    let packs = crate::api_surface::builtin_packs();
    let language = crate::api_surface::detect_source_language(prompt, &packs);
    let contains = |needle| crate::api_surface::contains_bounded_ascii(&lower, needle);
    let names_source_files = ["code file", "code files", "source file", "source files"]
        .iter()
        .any(|needle| contains(needle));
    let names_language_files = language.is_some()
        && ["file", "files", "script", "scripts"]
            .iter()
            .any(|needle| contains(needle));
    let source_files = names_source_files || names_language_files;

    let mut lines = Vec::new();
    if contains("table") {
        lines.push("response_shape: table".to_string());
    }

    if source_files {
        lines.push("evidence_scope: source_files".to_string());
        match language {
            Some(pack) => {
                if let Ok(extensions) =
                    crate::api_surface::source_extensions_for(&packs, Some(&pack.name))
                {
                    lines.push(format!("source_extensions: {}", extensions.join(",")));
                }
                lines.push(format!(
                    "source_filter: category=source language={}",
                    pack.name
                ));
            }
            None => lines.push("source_filter: category=source".to_string()),
        }
        lines.push(
            "scope_instruction: code/source means registered language source files only; \
             exclude documentation, manifests, lockfiles, and other repository metadata \
             from the primary evidence set"
                .to_string(),
        );
    }
    lines.join("\n")
}

/// The pure-data needle table driving [`infer_disposition`] (#1260, three-Cs):
/// the English phrase lists and the trailing-`?` fallback are LANGUAGE
/// knowledge, so they live in droppable/overridable data — the lexicon
/// convention (`api_surface.rs` language packs) — never hardcoded in logic.
/// Built-in defaults via [`Default`]; the `[intake]` config table overrides any
/// list wholesale and retargets the `?` fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispositionLexicon {
    /// Needles that force **Act** (checked first; any match wins).
    pub action: Vec<String>,
    /// Needles classifying **Research** (checked before explain).
    pub research: Vec<String>,
    /// Needles classifying **Explain**.
    pub explain: Vec<String>,
    /// Where a prompt that matches NO list but ends with `?` lands — the
    /// fallback cliff made visible and tunable (#1257: "What are the 10 largest
    /// Rust files…?" classified Explain SOLELY through this).
    pub question_mark_disposition: PromptDisposition,
    /// Clause openings that announce a stated fact outright (#1971). Matched at
    /// the START of a clause; no further evidence is required.
    pub informational_markers: Vec<String>,
    /// Declarative subject leads. Informational only when a
    /// [`stative_markers`](Self::stative_markers) entry also appears — see
    /// [`classify_clause`].
    pub declarative_leads: Vec<String>,
    /// Copulas and stative auxiliaries: the "X **is** Y" half of a statement.
    /// Matched as whole words.
    pub stative_markers: Vec<String>,
}

impl Default for DispositionLexicon {
    fn default() -> Self {
        Self {
            action: [
                "implement",
                "modify",
                "change",
                "create",
                "write",
                "edit",
                "delete",
                "fix",
                "build",
                "run ",
                "execute",
                // A quoted command after `test` is an execution request even
                // when phrased as a question. Keep this narrower than bare
                // `test ` so "explain how the test harness works" remains an
                // Explain deliverable.
                // Leading space is a word boundary. `infer_disposition_with`
                // pads the prompt so these also match at byte zero, without
                // widening `latest \"release\"` or `greatest 'risk'` to Act.
                " test \"",
                " test '",
                "commit",
                "push",
                "open a pr",
                "open pr",
                "merge",
            ]
            .map(str::to_string)
            .to_vec(),
            research: [
                "research",
                "investigate",
                "look up",
                "find out",
                "analyze",
                "diagnose",
                "audit",
                "explore",
                "compare",
                // #1260: evidence-gathering phrasings from the diagnosed #1257
                // session ("the 10 largest Rust files") — data additions, so
                // such prompts classify by CONTENT, not the `?` cliff.
                "largest",
                "biggest",
                "smallest",
                // #1387: line count is a first-class evidence question, answered
                // read-only by `find` (sort=lines/show_lines) — NOT an Act that
                // needs `wc -l`, and NOT a bytesize fallback. Keeping it in
                // Research and giving Research the capability is the fix for
                // "Research is too strict".
                "line count",
                "most lines",
                "fewest lines",
                "longest file",
                "shortest file",
            ]
            .map(str::to_string)
            .to_vec(),
            explain: [
                "explain",
                "summarize",
                "describe",
                "what is",
                "why ",
                "how does",
                "how do",
                // #1260: plural/interrogative forms the old list missed ("what
                // is" ≠ "what are" was half the #1257 cliff).
                "what are",
                "which are",
            ]
            .map(str::to_string)
            .to_vec(),
            question_mark_disposition: PromptDisposition::Explain,
            // Self-announcing statements. `i'll want` / `i'm going to` are the
            // future-tense forms only: bare `i want` is routinely an
            // instruction ("I want you to fix the parser") and is deliberately
            // absent.
            informational_markers: [
                "fyi",
                "btw",
                "by the way",
                "just so you know",
                "heads up",
                "note that",
                "for reference",
                "for context",
                "i'll want",
                "i will want",
                "i'm going to",
                "i am going to",
                "we're going to",
                "we are going to",
            ]
            .map(str::to_string)
            .to_vec(),
            // Subject leads. An English imperative does not open with one of
            // these, which is what keeps the 22 measured fallback imperatives
            // ("add…", "update…", "continue") out of this branch entirely.
            declarative_leads: [
                "the ", "a ", "an ", "this ", "that ", "these ", "those ", "it ", "its ", "it's ",
                "i ", "i'm ", "i've ", "i'll ", "we ", "we're ", "we've ", "we'll ", "you ",
                "your ", "there ", "my ", "our ", "their ", "his ", "her ",
            ]
            .map(str::to_string)
            .to_vec(),
            // Padded on both sides by `classify_clause`, so each matches as a
            // whole word rather than inside `revise` or `this`.
            stative_markers: [
                " is ",
                " are ",
                " was ",
                " were ",
                " isn't ",
                " aren't ",
                " will be ",
                " has ",
                " have ",
                " had ",
                "'s ",
                "'re ",
                "'ve ",
            ]
            .map(str::to_string)
            .to_vec(),
        }
    }
}

fn infer_disposition(prompt: &str, asks: &[AtomicAsk]) -> PromptDisposition {
    infer_disposition_with(prompt, asks, &DispositionLexicon::default())
}

/// Classify a prompt's disposition against `lexicon` (#1260) — pure, no I/O.
/// Precedence is unchanged from the historical logic: an action needle wins
/// outright; else research; else explain; else the `?` fallback; else the
/// terminal fallback.
///
/// # The terminal fallback no longer grants on silence (#1971)
///
/// It used to be `Act` unconditionally, which made *absence of evidence* the
/// most permissive disposition: a 92-byte statement of fact matched no needle,
/// fell through, and was handed the full execution authority and round budget
/// while the fact it stated was never acted on at all. That is the shape
/// #1908 named — "no match" and "matched as Act" are different facts, and
/// conflating them grants authority on silence.
///
/// It is now `Act` **unless every clause positively reads as a statement**, in
/// which case the turn is [`Explain`](PromptDisposition::Explain): answer,
/// acknowledge, read if useful, mutate nothing.
///
/// **`Act` remains the fallback for silence, and that is deliberate.** Of 22
/// ordinary imperatives measured against this lexicon — "add a test for the
/// parser", "update the docs", "rebase onto main", "continue", "land it" —
/// **all 22 reach `Act` only through this fallback**. Inverting it would strip
/// authorization from the ordinary case to fix the exceptional one. So the
/// narrowing is driven by evidence FOR a statement, never by the absence of
/// evidence for an instruction: silence still means Act, but a clause that
/// says "X is Y" no longer does.
///
/// A mixed prompt keeps `Act`. One instruction among five statements is still
/// an instruction, and `all()` over an empty ask list cannot arise —
/// [`extract_atomic_asks_with`] always yields at least one.
fn infer_disposition_with(
    prompt: &str,
    asks: &[AtomicAsk],
    lexicon: &DispositionLexicon,
) -> PromptDisposition {
    let lower = prompt.to_ascii_lowercase();
    // Padding makes a lexicon entry with a leading-space word boundary match at
    // the beginning of a prompt without losing that boundary inside prose.
    let padded = format!(" {lower}");
    let hit = |needles: &[String]| needles.iter().any(|n| !n.is_empty() && padded.contains(n));
    if hit(&lexicon.action) {
        return PromptDisposition::Act;
    }
    if hit(&lexicon.research) {
        return PromptDisposition::Research;
    }
    if hit(&lexicon.explain) {
        return PromptDisposition::Explain;
    }
    if lower.trim_end().ends_with('?') {
        return lexicon.question_mark_disposition;
    }
    if !asks.is_empty() && asks.iter().all(AtomicAsk::is_informational) {
        return PromptDisposition::Explain;
    }
    PromptDisposition::Act
}

fn extract_decisions(asks: &[AtomicAsk]) -> (Vec<DecisionLock>, bool) {
    let mut decisions = Vec::new();
    for ask in asks {
        let lower = ask.text.to_ascii_lowercase();
        // A claim such as "per policy" inside a prompt is not a verified
        // harness policy. This intake has no external policy resolver, so it
        // deliberately creates only unresolved decisions here. The only
        // automatic lock path in this MVP is a later explicit operator answer;
        // policy and authorized-assumption sources remain represented in the
        // durable schema for a future verified resolver.
        let decision =
            if needs_operator_decision(&lower) || has_ambiguous_destructive_target(&lower) {
                Some((DecisionStatus::Pending, None))
            } else {
                None
            };
        if let Some((status, source)) = decision {
            if decisions.len() == MAX_CONCRETE_DECISIONS {
                return (decisions, true);
            }
            decisions.push(DecisionLock {
                question: ask.text.clone(),
                status,
                source,
                assumption: None,
                overflow: false,
            });
        }
    }
    (decisions, false)
}

fn overflow_decision() -> DecisionLock {
    DecisionLock {
        question: format!(
            "This request exceeds Newt's bounded intake capacity ({MAX_CONCRETE_DECISIONS} decisions or {MAX_ATOMIC_ASKS} asks). Use /new, then start a smaller task before execution."
        ),
        status: DecisionStatus::Pending,
        source: None,
        assumption: None,
        overflow: true,
    }
}

/// Cues that mark a clause as a directive negation — "do not do X",
/// commanding the operator — rather than a descriptive one. Deliberately
/// excludes `"does not "` / `"doesn't "`: those are third-person indicative
/// ("the shim does not use SQLite or Postgres" describes behavior, it does
/// not command anything), and folding them into a "prohibition" cue list
/// would claim a mood this classifier cannot actually detect (#1708).
///
/// `"never "` and `"don't "` are kept even though English also permits an
/// indicative reading of both ("it never does X", "they don't do X"). This
/// classifier does not attempt that disambiguation beyond the interrogative
/// check in [`is_directive_prohibition`] — a rare indicative "never"/"don't"
/// clause that also contains " or " + a trigger word can still be misread
/// as a prohibition. #1689 item 6 tracks the general problem; #1689 item 5
/// tracks moving this and the sibling needle lists into the droppable
/// `DispositionLexicon`. Both are out of scope here — kept as a plain list
/// to keep this fix narrow.
const NEGATION_CUES: &[&str] = &[
    "do not ",
    "don't ",
    "must not ",
    "must never ",
    "should not ",
    "shouldn't ",
    "never ",
    "avoid ",
];

/// Leading words that can precede a directive without changing its mood: a
/// politeness filler or an explicit second-person subject. Stripped before
/// testing for a negation cue, so "Please do not X" and "You must not X"
/// are recognized the same as "Do not X" (#1708).
const DIRECTIVE_SUBJECT_PREFIXES: &[&str] = &["please ", "you "];

/// Strip a leading single-word "Label: " prefix — "Constraint:", "Rule:",
/// "Note:" — before testing for a negation cue (#1708). Generic on purpose:
/// instruction labels are free-form operator vocabulary, not a fixed phrase
/// list to keep in sync. Deliberately requires a single alphabetic word (no
/// spaces) immediately before the colon so an unrelated mid-clause colon —
/// "the old value was: never mind" — is never mistaken for a label.
fn strip_leading_label(s: &str) -> &str {
    if let Some(colon) = s.find(':') {
        let (label, rest) = s.split_at(colon);
        if (2..=20).contains(&label.len())
            && label.chars().all(|c| c.is_ascii_alphabetic())
            && rest.starts_with(": ")
        {
            return rest[": ".len()..].trim_start();
        }
    }
    s
}

/// Is `lower` (an already-lowercased atomic ask) a directive prohibition —
/// a negated command to the operator — rather than a question or a
/// descriptive statement?
///
/// A question mark is decisive either way: "Shouldn't we use X or Y?" opens
/// with a negated auxiliary but is asking, not commanding, so a `?`
/// anywhere in the clause vetoes prohibition classification outright. This
/// is intentionally cruder than parsing subject-auxiliary inversion — it
/// will also (wrongly) veto a genuine prohibition that ends in a
/// rhetorical "...understood?", which is an accepted, narrow limitation
/// (#1708) rather than a claim this function does not make.
fn is_directive_prohibition(lower: &str) -> bool {
    if lower.contains('?') {
        return false;
    }
    let mut clause = strip_leading_label(lower.trim_start());
    while let Some(rest) = DIRECTIVE_SUBJECT_PREFIXES
        .iter()
        .find_map(|prefix| clause.strip_prefix(prefix))
    {
        clause = rest;
    }
    NEGATION_CUES.iter().any(|cue| clause.starts_with(cue))
}

/// A clause needs an explicit operator decision only when it poses a real
/// choice. "Do NOT implement A, B, or C" and "Never select A or B" name a
/// prohibition, not a question — [`is_directive_prohibition`] gates both
/// the `choose`/`select`/`pick` needles and the `" or "` + trigger-word
/// heuristic below so a prohibited clause cannot trip either (#1707,
/// #1708). It intentionally does NOT gate `"either "`/`"tbd"`/`"which
/// ..."`: a negated form of those is rare and out of scope for this fix.
fn needs_operator_decision(lower: &str) -> bool {
    let prohibition = is_directive_prohibition(lower);
    // The detector stays conservative: it generates CANDIDATES. Whether the
    // operator delegated a candidate is decided by bounded adjudication, not by
    // making these needles cleverer (#1749).
    (!prohibition && contains_any(lower, &["choose ", "select ", "pick "]))
        || contains_any(
            lower,
            &[
                "either ",
                "tbd",
                "to be decided",
                "which option",
                "which backend",
                "which provider",
            ],
        )
        || (!prohibition
            && lower.contains(" or ")
            && contains_any(lower, &["should", "use", "implement"]))
}

/// A destructive verb with a bare demonstrative/pronoun has no grounded
/// target. Treat it as a blocking decision rather than inheriting whatever
/// object happened to be salient in the model's transient context.
fn has_ambiguous_destructive_target(lower: &str) -> bool {
    [
        "delete it",
        "delete this",
        "delete that",
        "remove it",
        "remove this",
        "remove that",
        "drop it",
        "drop this",
        "drop that",
        "destroy it",
        "destroy this",
        "destroy that",
        "wipe it",
        "wipe this",
        "wipe that",
        "purge it",
        "purge this",
        "purge that",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// Why an operator's clarification reply did not lock the batch.
///
/// #1689 item 1. The gate used to re-emit a byte-identical block on every
/// rejection, which is what made a blocked session indistinguishable from a
/// hung one: nothing said what was wrong, and nothing named the way out. The
/// model is never called on this path, so if the harness does not explain the
/// refusal, nothing will.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarificationRejection {
    /// Nothing that parsed as `N: value` appeared anywhere in the reply.
    NoOrdinals,
    /// Ordinals parsed, but not for every pending decision.
    Incomplete { answered: usize, expected: usize },
    /// An ordinal outside the pending range was used.
    OutOfRange { ordinal: usize, expected: usize },
    /// The reply read as a question rather than an answer, and carried no
    /// ordinals to override that reading.
    ReadsAsQuestion,
}

impl ClarificationRejection {
    /// One line stating what was wrong and how to get out — always naming the
    /// escape hatch, because the operator's real problem is usually that they
    /// disagree with the gate rather than that they mistyped.
    pub fn explain(&self) -> String {
        let detail = match self {
            Self::NoOrdinals => {
                "no `N: value` line was found — each answer needs its ordinal, like `1: use the second option`".to_string()
            }
            Self::Incomplete { answered, expected } => format!(
                "{answered} of {expected} decisions were answered — locking is all-or-nothing, so answer every ordinal in one reply"
            ),
            Self::OutOfRange { ordinal, expected } => format!(
                "ordinal {ordinal} is outside this batch — the pending items are numbered 1..={expected}"
            ),
            Self::ReadsAsQuestion => {
                "the reply read as a question rather than an answer — prefix each answer with its ordinal (`1: …`) and it will be accepted even if it contains a `?`".to_string()
            }
        };
        format!(
            "that reply did not lock the batch: {detail}.\n\
             (`/new` abandons this prompt and starts a fresh conversation.)"
        )
    }
}

/// Resolve explicit operator answers, or say why the reply was refused.
///
/// #1689 items 1 and 2.
fn explicit_answer_outcome(
    answer: &str,
    pending: &[usize],
) -> Result<Vec<usize>, ClarificationRejection> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Err(ClarificationRejection::NoOrdinals);
    }
    let mut resolved = BTreeSet::new();
    let mut saw_ordinal = false;
    let mut out_of_range = None;
    for line in answer.lines() {
        let line = line.trim();
        let line = line.strip_prefix("decision ").unwrap_or(line);
        let Some((ordinal, value)) = line.split_once(':') else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        let Ok(ordinal) = ordinal.trim().parse::<usize>() else {
            continue;
        };
        saw_ordinal = true;
        let Some(pending_ordinal) = ordinal.checked_sub(1) else {
            out_of_range = Some(ordinal);
            continue;
        };
        match pending.get(pending_ordinal) {
            Some(index) => {
                resolved.insert(*index);
            }
            None => out_of_range = Some(ordinal),
        }
    }

    // #1689 item 2: the question heuristic applies ONLY when the reply carried
    // no usable ordinals. It used to reject the WHOLE answer for a `?` anywhere
    // in it, so `1: drain before rotation — sound right?` was refused: a
    // perfectly good answer with a thought attached. An explicit `N: value` is
    // the operator stating a decision, and that outranks a punctuation guess.
    if !saw_ordinal {
        if looks_like_unresolved_question(answer) {
            return Err(ClarificationRejection::ReadsAsQuestion);
        }
        return Err(ClarificationRejection::NoOrdinals);
    }
    if let Some(ordinal) = out_of_range {
        if resolved.len() != pending.len() {
            return Err(ClarificationRejection::OutOfRange {
                ordinal,
                expected: pending.len(),
            });
        }
    }
    if resolved.len() != pending.len() {
        return Err(ClarificationRejection::Incomplete {
            answered: resolved.len(),
            expected: pending.len(),
        });
    }
    Ok(resolved.into_iter().collect())
}

/// The pre-#1689 shape, kept for the existing parser tests: outcome without a
/// reason. Production reads `explicit_answer_outcome` so a refusal can be
/// explained instead of silently repeated.
#[cfg(test)]
fn explicit_answer_indices(answer: &str, pending: &[usize]) -> Option<Vec<usize>> {
    explicit_answer_outcome(answer, pending).ok()
}

fn looks_like_unresolved_question(answer: &str) -> bool {
    let lower = answer.to_ascii_lowercase();
    answer.contains('?')
        || lower.starts_with("what ")
        || lower.starts_with("which ")
        || lower.starts_with("can ")
        || lower.starts_with("could ")
        || lower.starts_with("should ")
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn truncate_chars(text: &str, byte_limit: usize) -> String {
    if text.len() <= byte_limit {
        return text.to_string();
    }
    let mut end = byte_limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn digest_metadata(text: &str) -> Value {
    json!({
        "digest": blake3::hash(text.as_bytes()).to_hex().to_string(),
        "bytes": text.len() as u64,
    })
}

#[cfg(test)]
#[path = "prompt_intake_tests/mod.rs"]
mod prompt_intake_tests;
