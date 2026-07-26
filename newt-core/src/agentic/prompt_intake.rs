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
const RESEARCH_TOOL_ROUND_LIMIT: usize = 3;
pub(super) const PROMPT_COMPREHENSION_SCHEMA_V1: &str = "prompt_comprehension_manifest_v1";
pub(super) const PROMPT_COMPREHENSION_SCHEMA_V2: &str = "prompt_comprehension_manifest_v2";
pub(super) const PROMPT_COMPREHENSION_SCHEMA_CURRENT: &str = PROMPT_COMPREHENSION_SCHEMA_V2;

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

/// One bounded clause extracted from a monolithic prompt. Text remains only in
/// memory and in the durable prompt receipt; artifact metadata stores a digest
/// and byte count instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicAsk {
    text: String,
}

impl AtomicAsk {
    pub fn text(&self) -> &str {
        &self.text
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
        // Re-derive only the lexicon-driven part; asks/decisions are unchanged.
        intake.post_lock_disposition = infer_disposition_with(prompt, lexicon);
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
                    }],
                    decisions: vec![DecisionLock {
                        question: "Provide a non-empty task before execution.".to_string(),
                        status: DecisionStatus::Pending,
                        source: None,
                        overflow: false,
                    }],
                },
                disposition: PromptDisposition::Ask,
                post_lock_disposition: PromptDisposition::Explain,
            };
            debug_assert!(intake.validate().is_ok());
            return intake;
        }
        let (atomic_asks, atomic_overflow) = extract_atomic_asks(prompt);
        let post_lock_disposition = infer_disposition(prompt);
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
        }
        debug_assert!(self.validate().is_ok());
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
        let pending = self
            .manifest
            .decisions
            .iter()
            .enumerate()
            .filter(|(_, decision)| decision.status == DecisionStatus::Pending)
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return String::new();
        }

        let mut rendered = String::from(
            "I need these decisions locked before I can execute. Reply using an explicit ordinal for every item, for example `1: …`:\n",
        );
        for (ordinal, decision) in pending {
            let question = truncate_chars(&decision.question, MAX_CLARIFICATION_BYTES);
            rendered.push_str(&format!("{}. {}\n", ordinal + 1, question));
        }
        rendered.trim_end().to_string()
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
        let pending = resolved
            .manifest
            .decisions
            .iter()
            .enumerate()
            .filter_map(|(index, decision)| {
                (decision.status == DecisionStatus::Pending).then_some(index)
            })
            .collect::<Vec<_>>();

        if let Some(indices) = explicit_answer_indices(answer, &pending) {
            for index in indices {
                let decision = &mut resolved.manifest.decisions[index];
                decision.status = DecisionStatus::Locked;
                decision.source = Some(DecisionSource::Operator);
            }
        }

        resolved.disposition = if resolved.manifest.pending_decision_count() == 0 {
            resolved.post_lock_disposition
        } else {
            PromptDisposition::Ask
        };
        debug_assert!(resolved.validate().is_ok());
        resolved
    }

    /// Content-free model projection placed inside the protected active-prompt
    /// card. It contains no prompt, decision, or clarification text.
    pub fn model_card(&self) -> String {
        let pending = self.manifest.pending_decision_count();
        let locked = self.manifest.locked_decision_count();
        let instruction = match self.disposition {
            PromptDisposition::Ask => {
                "harness_action: await the bounded operator clarification; do not call tools"
            }
            PromptDisposition::Act => {
                "harness_action: decisions are locked; ordinary execution authority is available"
            }
            PromptDisposition::Explain => {
                "harness_action: answer without mutation; bounded read/recovery tools only"
            }
            PromptDisposition::Research => {
                "harness_action: gather bounded read-only evidence; do not mutate or request capability grants"
            }
            PromptDisposition::Plan => {
                "harness_action: read evidence and maintain the harness plan ledger only; do not mutate the workspace, execute commands, or request capability grants"
            }
        };
        format!(
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
        )
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

fn extract_atomic_asks(prompt: &str) -> (Vec<AtomicAsk>, bool) {
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
                });
            }
        }
    }
    if asks.is_empty() {
        asks.push(AtomicAsk {
            text: "(empty operator prompt)".to_string(),
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
        }
    }
}

fn infer_disposition(prompt: &str) -> PromptDisposition {
    infer_disposition_with(prompt, &DispositionLexicon::default())
}

/// Classify a prompt's disposition against `lexicon` (#1260) — pure, no I/O.
/// Precedence is unchanged from the historical logic: an action needle wins
/// outright; else research; else explain; else the `?` fallback; else Act.
fn infer_disposition_with(prompt: &str, lexicon: &DispositionLexicon) -> PromptDisposition {
    let lower = prompt.to_ascii_lowercase();
    let hit = |needles: &[String]| needles.iter().any(|n| !n.is_empty() && lower.contains(n));
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
        overflow: true,
    }
}

fn needs_operator_decision(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "either ",
            "choose ",
            "select ",
            "pick ",
            "tbd",
            "to be decided",
            "which option",
            "which backend",
            "which provider",
        ],
    ) || (lower.contains(" or ") && contains_any(lower, &["should", "use", "implement"]))
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

fn explicit_answer_indices(answer: &str, pending: &[usize]) -> Option<Vec<usize>> {
    let answer = answer.trim();
    if answer.is_empty() || looks_like_unresolved_question(answer) {
        return None;
    }
    let mut resolved = BTreeSet::new();
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
        let pending_ordinal = ordinal.checked_sub(1)?;
        if let Some(index) = pending.get(pending_ordinal) {
            resolved.insert(*index);
        }
    }
    (resolved.len() == pending.len()).then(|| resolved.into_iter().collect())
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
mod tests {
    use super::{
        DispositionLexicon, PromptDisposition, PromptIntake, MAX_ATOMIC_ASKS,
        MAX_CONCRETE_DECISIONS, PROMPT_COMPREHENSION_MODEL_CARD_PREFIX,
    };

    #[test]
    fn action_prompt_is_atomic_and_metadata_is_content_free() {
        let secret = "ship the private parser change to /top-secret";
        let intake = PromptIntake::analyze(secret);

        assert_eq!(intake.disposition(), PromptDisposition::Act);
        assert_eq!(intake.atomic_asks().len(), 1);
        intake.validate().unwrap();
        let artifact_metadata = intake.artifact_metadata();
        assert_eq!(
            artifact_metadata["schema"],
            "prompt_comprehension_manifest_v2"
        );
        let metadata = artifact_metadata.to_string();
        assert!(!metadata.contains("private parser"));
        assert!(metadata.contains("atomic_ask_digests"));
        let card = intake.model_card();
        assert!(card.starts_with(PROMPT_COMPREHENSION_MODEL_CARD_PREFIX));
        assert!(!card.contains("private parser"));
    }

    #[test]
    fn empty_headless_input_is_a_bounded_ask_not_act() {
        let empty = PromptIntake::analyze("   \n");
        assert_eq!(empty.disposition(), PromptDisposition::Ask);
        assert_eq!(empty.manifest().pending_decision_count(), 1);
        assert!(empty.clarification_batch().contains("non-empty task"));
        assert_eq!(
            empty
                .resolve_with_operator_answer("Explain receipts.")
                .disposition(),
            PromptDisposition::Explain
        );
    }

    #[test]
    fn unresolved_choice_becomes_a_bounded_ask_then_explicit_answer_acts() {
        let intake = PromptIntake::analyze(
            "Implement either SQLite or Postgres; create the migration and open a PR.",
        );

        assert_eq!(intake.disposition(), PromptDisposition::Ask);
        assert_eq!(intake.atomic_asks().len(), 2);
        assert!(intake.clarification_batch().contains("SQLite"));
        assert_eq!(
            intake
                .resolve_with_operator_answer("continue")
                .disposition(),
            PromptDisposition::Ask,
            "an acknowledgement cannot choose a concrete implementation"
        );
        let resolved = intake.resolve_with_operator_answer("1: SQLite");
        assert_eq!(resolved.disposition(), PromptDisposition::Act);
        assert_eq!(resolved.manifest().pending_decision_count(), 0);
        assert_eq!(
            resolved.artifact_metadata()["decision_source_counts"]["operator"],
            1
        );
        resolved.validate().unwrap();
    }

    #[test]
    fn multiple_decisions_require_explicit_ordinal_mapping() {
        let intake = PromptIntake::analyze(
            "Choose either SQLite or Postgres. Select either staging or production.",
        );
        assert_eq!(intake.manifest().pending_decision_count(), 2);
        assert_eq!(
            intake
                .resolve_with_operator_answer("SQLite\nproduction")
                .disposition(),
            PromptDisposition::Ask
        );
        assert_eq!(
            intake
                .resolve_with_operator_answer("1: SQLite\n2: production")
                .disposition(),
            PromptDisposition::Act
        );
    }

    #[test]
    fn intake_overflow_remains_ask_and_cannot_be_answered_in_place() {
        let prompt = (0..=MAX_CONCRETE_DECISIONS)
            .map(|i| format!("Choose either option-{i}-a or option-{i}-b."))
            .collect::<Vec<_>>()
            .join("\n");
        let intake = PromptIntake::analyze(&prompt);

        assert_eq!(intake.disposition(), PromptDisposition::Ask);
        assert!(
            intake
                .manifest()
                .decisions()
                .iter()
                .any(super::DecisionLock::is_overflow),
            "a truncated decision set must retain an explicit overflow lock"
        );
        assert_eq!(
            intake
                .resolve_with_operator_answer("1: option-0-a")
                .disposition(),
            PromptDisposition::Ask,
            "the overflow lock cannot be converted into Act by a partial answer"
        );
    }

    #[test]
    fn atomic_ask_overflow_remains_ask() {
        let prompt = (0..=MAX_ATOMIC_ASKS)
            .map(|i| format!("Implement bounded item {i}."))
            .collect::<Vec<_>>()
            .join("\n");
        let intake = PromptIntake::analyze(&prompt);

        assert_eq!(intake.atomic_asks().len(), MAX_ATOMIC_ASKS);
        assert_eq!(intake.disposition(), PromptDisposition::Ask);
        assert!(intake.manifest().decisions().iter().any(|decision| {
            decision.is_overflow() && decision.status() == super::DecisionStatus::Pending
        }));
    }

    #[test]
    fn ambiguous_destructive_pronoun_requires_clarification() {
        let ambiguous = PromptIntake::analyze("Delete it.");
        assert_eq!(ambiguous.disposition(), PromptDisposition::Ask);

        let grounded = PromptIntake::analyze("Delete scratch/obsolete.txt.");
        assert_eq!(grounded.disposition(), PromptDisposition::Act);
    }

    #[test]
    fn explain_and_research_receive_their_intended_bounded_tool_loops() {
        let explain = PromptIntake::analyze("Explain how prompt receipts survive compaction.");
        let research = PromptIntake::analyze("Investigate the current compaction behavior.");

        assert_eq!(explain.disposition(), PromptDisposition::Explain);
        assert_eq!(research.disposition(), PromptDisposition::Research);
        assert_eq!(PromptDisposition::Ask.tool_round_limit(40), 0);
        assert_eq!(PromptDisposition::Explain.tool_round_limit(40), 40);
        assert_eq!(PromptDisposition::Research.tool_round_limit(40), 3);
        assert_eq!(PromptDisposition::Plan.tool_round_limit(40), 40);
    }

    #[test]
    fn read_only_attenuation_keeps_model_card_and_artifact_in_sync() {
        let mut action = PromptIntake::analyze("Implement the requested parser change.");
        assert_eq!(action.disposition(), PromptDisposition::Act);

        action.enforce_read_only(PromptDisposition::Plan);

        assert_eq!(action.disposition(), PromptDisposition::Plan);
        assert!(
            action.model_card().contains("disposition: plan"),
            "{}",
            action.model_card()
        );
        assert_eq!(
            action.artifact_metadata()["schema"],
            "prompt_comprehension_manifest_v2"
        );
        assert_eq!(action.artifact_metadata()["disposition"], "plan");

        let mut research = PromptIntake::analyze("Investigate the parser behavior.");
        research.enforce_read_only(PromptDisposition::Research);
        assert_eq!(
            research.disposition(),
            PromptDisposition::Research,
            "the mode-selected read-only disposition must remain consistent"
        );
    }

    #[test]
    fn ordinal_answers_are_relative_to_the_pending_batch() {
        assert_eq!(
            super::explicit_answer_indices("1: continue", &[4]),
            Some(vec![4]),
            "the first displayed clarification must resolve the first pending decision, not raw decision zero"
        );
        assert_eq!(
            super::explicit_answer_indices("1: one\n2: two", &[2, 6]),
            Some(vec![2, 6])
        );
    }

    // ── #1260: disposition inference as pure data ───────────────────────────

    /// The #1257 canonical prompt. Today's defaults classify it Research by
    /// CONTENT ("largest" is evidence-phrasing data) — not the `?` cliff.
    const LARGEST_FILES_PROMPT: &str = "What are the 10 largest Rust files in this workspace?";

    /// The pre-#1260 lists, reconstructed as an override — documents the cliff
    /// durably: under the OLD data this prompt matched NOTHING ("what is" ≠
    /// "what are"; research had "find out", not "largest") and was classified
    /// Explain SOLELY by the trailing `?`, while the identical prompt minus its
    /// `?` fell to Act. Any future change to this coupling is now deliberate.
    fn pre_1260_lexicon() -> DispositionLexicon {
        DispositionLexicon {
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
            ]
            .map(str::to_string)
            .to_vec(),
            question_mark_disposition: PromptDisposition::Explain,
        }
    }

    #[test]
    fn largest_files_question_classified_explain_via_question_mark_fallback_pre_1260() {
        let old = pre_1260_lexicon();
        assert_eq!(
            super::infer_disposition_with(LARGEST_FILES_PROMPT, &old),
            PromptDisposition::Explain,
            "under the OLD data the ? fallback alone decided"
        );
        assert_eq!(
            super::infer_disposition_with(LARGEST_FILES_PROMPT.trim_end_matches('?'), &old),
            PromptDisposition::Act,
            "…and the same prompt minus its ? fell off the cliff to Act"
        );
    }

    #[test]
    fn new_defaults_classify_evidence_questions_by_content_not_the_cliff() {
        // "largest" (research data) decides — with or without the `?`.
        let with_q = PromptIntake::analyze(LARGEST_FILES_PROMPT);
        assert_eq!(with_q.disposition(), PromptDisposition::Research);
        let without_q =
            PromptIntake::analyze("What are the 10 largest Rust files in this workspace");
        assert_eq!(
            without_q.disposition(),
            PromptDisposition::Research,
            "content decides; removing the ? no longer flips the disposition"
        );
        // "what are" (explain data) catches the plural interrogative the old
        // list missed.
        let plural = PromptIntake::analyze("What are the tradeoffs of this design?");
        assert_eq!(plural.disposition(), PromptDisposition::Explain);
        // A bare statement matching nothing still defaults to Act.
        let act = PromptIntake::analyze("update the release notes for 0.8.0");
        assert_eq!(act.disposition(), PromptDisposition::Act);
    }

    #[test]
    fn line_count_questions_classify_research_not_the_cliff() {
        // #1387: the regressed prompt. "line count" is evidence phrasing, so it
        // lands in Research — where `find` (sort=lines/show_lines) can answer it
        // read-only. It must NOT fall off the `?` cliff to Explain, and must NOT
        // require Act (a mutation grant) just to count lines.
        let regressed =
            PromptIntake::analyze("show me the 10 code files with the highest line counts?");
        assert_eq!(
            regressed.disposition(),
            PromptDisposition::Research,
            "line-count question is a Research/evidence turn, not Explain or Act"
        );
        for prompt in [
            "which files have the most lines",
            "the longest file in the repo",
            "files with the fewest lines",
        ] {
            assert_eq!(
                PromptIntake::analyze(prompt).disposition(),
                PromptDisposition::Research,
                "line-count evidence phrasing → Research: {prompt:?}"
            );
        }
    }

    #[test]
    fn lexicon_overrides_drive_inference_table_driven() {
        // A dropped-in override list REPLACES its default wholesale.
        let custom = DispositionLexicon {
            explain: vec!["kerfuffle".to_string()],
            question_mark_disposition: PromptDisposition::Research,
            ..DispositionLexicon::default()
        };
        for (prompt, want) in [
            ("tell me about the kerfuffle", PromptDisposition::Explain),
            // The default explain needles are GONE (replaced), so "what is…?"
            // now reaches the retargeted ? fallback → Research.
            ("what is a monad?", PromptDisposition::Research),
            // Action still wins outright.
            ("fix the kerfuffle", PromptDisposition::Act),
            // No needle, no ?: Act.
            ("status report", PromptDisposition::Act),
        ] {
            assert_eq!(
                super::infer_disposition_with(prompt, &custom),
                want,
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn analyze_with_applies_the_lexicon_and_keeps_ask_precedence() {
        // The lexicon changes the classification vs the defaults…
        let lex = DispositionLexicon {
            research: vec!["kerfuffle".to_string()],
            ..DispositionLexicon::default()
        };
        let intake = PromptIntake::analyze_with("tell me about the kerfuffle", &lex);
        assert_eq!(intake.disposition(), PromptDisposition::Research);
        // …but an unresolved decision still forces the Ask terminal, with the
        // lexicon-derived value preserved as the post-lock disposition.
        let asky = PromptIntake::analyze_with(
            "Investigate either the kerfuffle or the brouhaha; compare them.",
            &lex,
        );
        if asky.manifest().pending_decision_count() > 0 {
            assert_eq!(asky.disposition(), PromptDisposition::Ask);
        }
        // The empty-prompt Ask terminal is untouched by any lexicon.
        let empty = PromptIntake::analyze_with("   ", &lex);
        assert_eq!(empty.disposition(), PromptDisposition::Ask);
    }
}
