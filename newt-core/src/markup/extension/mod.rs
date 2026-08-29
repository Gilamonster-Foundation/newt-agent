//! **Fenced-block extensions: a contract in which source is never lost.**
//!
//! E0a of epic #1803. A fenced block whose info string names an extension —
//! ```` ```mermaid ```` today — may be presented as something richer than a
//! code block. This module decides *whether* that is allowed and *within what
//! bounds*; it renders no graphics itself, and it is the only place the
//! decision is made.
//!
//! ## The one law: "no usable result" and "source" are the same path
//!
//! [`Presentation`] carries the source as a FIELD, not as one arm of an enum.
//! There is no way to obtain a presentation that lacks it, and no `Result`
//! anywhere in the entry point — so falling back is not a branch a caller can
//! forget to write, or a handler can fail to trigger. An enhancement is an
//! *optional extra riding on top of* the source, never an alternative to it.
//!
//! This shape is not aesthetic. C3b (#1861) measured what the alternative
//! costs: with a strict `style-src-elem`, Mermaid's per-render theme
//! stylesheet is blocked, and the diagram then renders with black fill AND
//! black text — unreadable, not merely unstyled, and silently so, because the
//! acceptance test asserted a diagram was *present* rather than legible.
//! **"Graphics unavailable" must resolve to source, never to a degraded
//! render.** A design where the fallback is the error path cannot express
//! that: a handler that produced *something* has not errored.
//!
//! ## What a handler may say
//!
//! A handler returns [`Option<Enhancement>`]. There is deliberately no way to
//! say "I produced something, but badly": [`Enhancement`]'s fields are private
//! and its constructors validate, so a malformed enhancement is
//! unrepresentable rather than merely discouraged. A handler that is unhappy
//! returns `None` and the reader gets the source.
//!
//! ## Where budgets live
//!
//! In the registry, checked **before** the handler is called — not inside each
//! handler. A budget every future registrant has to remember is a budget one
//! of them will not, and the failure is silent. [`Extension::measure`] is a
//! cheap, pure scan reporting a [`Shape`]; [`Budgets`] are applied to it, and
//! a refusal returns the source without the handler running at all.
//!
//! ## Purity
//!
//! A handler takes `&str` and returns `Option<Enhancement>`. It receives no
//! filesystem, no network, no process, no clock and no `&mut` anything, so the
//! *signature* admits no side effect. Signatures alone cannot stop a handler
//! reaching for `std::fs` internally, so the structural half is completed by
//! `newt-core/tests/extension_purity.rs`, which scans this module's production
//! source for ambient authority and carries an anti-vacuous twin.
//!
//! Nothing here evaluates diagram-authored input. A handler *reads* source
//! text and *emits* a representation; the source is data at every step.
//!
//! ## Output is untrusted
//!
//! [`Enhancement::payload`] is generated markup, and generating it here does
//! not make it trustworthy — a payload derived from author-controlled source
//! is author-influenced. [`Enhancement::level`] tells a surface what it is
//! looking at, and the contract is that a surface MUST sanitize a
//! [`SupportLevel::Graphics`] payload before it reaches a page. The sanitizer
//! lives in `newt-web` and is not this crate's to hold; what this crate can do
//! is refuse to hand out a payload without saying what it is, which is why
//! there is no `&str` accessor that yields markup without its level.
//!
//! **This contract does not require the extension marker before sanitization.**
//! #1848 fixed marker forgery by sanitizing FIRST and wrapping after, so the
//! marker is absent from the sanitizer's input and cannot be forged. Nothing
//! here asks a surface to invert that: [`Enhancement`] carries a payload and a
//! level, and where a surface puts its own marker is the surface's business.

pub mod mermaid;

use std::collections::BTreeMap;

/// How richly a fenced block can be presented.
///
/// Ordered deliberately: a surface declares the highest level it can honour,
/// and anything above it is simply unavailable — which resolves to source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SupportLevel {
    /// The fence's own text, rendered as a code block. Always available, on
    /// every surface, and the reason this enum has a floor rather than an
    /// `Option`.
    Source,
    /// A textual rendering — an ASCII/box projection for a terminal.
    Text,
    /// A graphical rendering — SVG for a browser.
    Graphics,
}

/// Why a block is being shown as source.
///
/// Carried so a surface can TELL the reader, which is the difference between
/// a fallback and a silent degradation. C3b's black-on-black diagram was
/// invisible precisely because nothing said "this is not what you think".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// No extension is registered for this info string.
    Unregistered,
    /// The surface cannot honour any level this extension offers.
    SurfaceCannot,
    /// A budget refused it before the handler ran.
    OverBudget(Budget),
    /// The handler declined — it had no usable result to offer.
    HandlerDeclined,
    /// The handler produced a result that exceeded the output budget, or took
    /// longer than the time budget. The result is discarded, not truncated:
    /// half a diagram is a degraded render, which is the thing this contract
    /// exists to prevent.
    ResultRefused(Budget),
}

/// Which budget refused. Each is enforced and tested independently, so a
/// refusal names the bound that fired rather than "too big".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Bytes of fence source.
    SourceBytes,
    /// Nodes the extension reported.
    Nodes,
    /// Edges the extension reported.
    Edges,
    /// Structural nesting depth the extension reported.
    Depth,
    /// Bytes of generated payload.
    OutputBytes,
    /// Nanoseconds spent in the handler.
    Time,
}

/// The bounds a registrant runs under.
///
/// Every field is a hard ceiling and each is checked separately, so a test can
/// drive one to its boundary without tripping another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// Maximum bytes of fence source.
    pub source_bytes: usize,
    /// Maximum nodes.
    pub nodes: usize,
    /// Maximum edges.
    pub edges: usize,
    /// Maximum nesting depth.
    pub depth: usize,
    /// Maximum bytes of generated payload.
    pub output_bytes: usize,
    /// Maximum nanoseconds in the handler.
    pub time_nanos: u128,
}

/// What a cheap pre-scan reports about a source, for the registry to judge.
///
/// Reported by the extension because counting nodes is syntax-specific, and
/// judged by the registry because enforcement must not be per-handler. A
/// registrant that under-reports can only reach the handler; the output and
/// time budgets still bound what it can produce, and purity bounds what it can
/// do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shape {
    /// Nodes the source declares.
    pub nodes: usize,
    /// Edges the source declares.
    pub edges: usize,
    /// Deepest structural nesting.
    pub depth: usize,
}

/// A monotonic nanosecond reading, injected so the time budget is testable
/// without asserting a real elapsed duration.
///
/// **Not [`crate::timer::Clock`]**, which answers "what unix SECOND is it" for
/// scheduling. A render budget needs elapsed sub-millisecond time, and giving
/// the scheduling clock a `now_nanos` would put a method on `SystemClock` that
/// timers have no use for, to answer a different question. Two narrow traits
/// beat one that means two things.
pub trait Stopwatch {
    /// A monotonically non-decreasing reading, in nanoseconds.
    fn read_nanos(&self) -> u128;
}

/// The real monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct Monotonic(std::time::Instant);

impl Default for Monotonic {
    fn default() -> Self {
        Self(std::time::Instant::now())
    }
}

impl Stopwatch for Monotonic {
    fn read_nanos(&self) -> u128 {
        self.0.elapsed().as_nanos()
    }
}

/// A rendering a handler produced, validated on the way in.
///
/// Private fields and validating constructors are the point: there is no way
/// to build one that is empty, or that claims [`SupportLevel::Source`] (source
/// is not an enhancement — it is what you already have), or that omits the
/// accessible text a non-visual reader needs. A handler cannot express
/// "something, but badly", so the registry never has to decide whether a
/// half-formed result is good enough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enhancement {
    level: SupportLevel,
    payload: String,
    accessible_text: String,
}

impl Enhancement {
    /// A textual rendering, with the text a screen reader is given.
    ///
    /// Returns `None` — never an error — when the payload or the accessible
    /// text is empty. `None` is the handler's own "I have nothing usable",
    /// and it lands on the same path as every other fallback.
    #[must_use]
    pub fn text(payload: impl Into<String>, accessible_text: impl Into<String>) -> Option<Self> {
        Self::build(SupportLevel::Text, payload.into(), accessible_text.into())
    }

    /// A graphical rendering, with its adjacent accessible text.
    ///
    /// The accessible text is REQUIRED, not optional. A diagram with no text
    /// alternative is unreadable to a screen reader in exactly the way C3b's
    /// black-on-black diagram was unreadable to everyone — and the fallback
    /// this contract guarantees is strictly better than an unlabelled image.
    #[must_use]
    pub fn graphics(
        payload: impl Into<String>,
        accessible_text: impl Into<String>,
    ) -> Option<Self> {
        Self::build(
            SupportLevel::Graphics,
            payload.into(),
            accessible_text.into(),
        )
    }

    fn build(level: SupportLevel, payload: String, accessible_text: String) -> Option<Self> {
        (!payload.trim().is_empty() && !accessible_text.trim().is_empty()).then_some(Self {
            level,
            payload,
            accessible_text,
        })
    }

    /// What this is, so a surface knows what it must do with it.
    #[must_use]
    pub fn level(&self) -> SupportLevel {
        self.level
    }

    /// The generated markup. **Untrusted**: derived from author-controlled
    /// source, so a surface sanitizes it before it reaches a page.
    #[must_use]
    pub fn payload(&self) -> &str {
        &self.payload
    }

    /// The adjacent text a non-visual reader is given instead.
    #[must_use]
    pub fn accessible_text(&self) -> &str {
        &self.accessible_text
    }
}

/// One registrant.
///
/// Object-safe on purpose: the registry holds `&'static dyn Extension`, so a
/// surface composes registrants without this module knowing them.
pub trait Extension: Send + Sync {
    /// The fence info string this claims, lowercase.
    fn info(&self) -> &'static str;

    /// The bounds this runs under.
    fn budgets(&self) -> Budgets;

    /// A cheap, pure pre-scan. Called BEFORE [`Extension::render`], and its
    /// result is what the registry judges — so a source over budget never
    /// reaches the renderer.
    fn measure(&self, source: &str) -> Shape;

    /// Produce a rendering at `level`, or `None` for "nothing usable".
    ///
    /// Pure: no filesystem, no network, no process, no evaluation of anything
    /// the source says. `newt-core/tests/extension_purity.rs` holds that
    /// structurally.
    fn render(&self, source: &str, level: SupportLevel) -> Option<Enhancement>;
}

/// What a surface can honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The richest level this surface can present.
    pub highest: SupportLevel,
}

impl Capabilities {
    /// A surface that can only show source — the headless and plain tiers, and
    /// any surface whose richer path is unavailable at runtime (C3b's web
    /// under a strict `style-src-elem`, for instance).
    #[must_use]
    pub const fn source_only() -> Self {
        Self {
            highest: SupportLevel::Source,
        }
    }
}

/// What a surface receives.
///
/// The source is a FIELD. There is no constructor that omits it, and no arm of
/// anything that replaces it — so "render the source" is not a path a caller
/// chooses, it is the floor every presentation already stands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation<'a> {
    info: &'a str,
    source: &'a str,
    enhancement: Option<Enhancement>,
    fallback: Option<FallbackReason>,
}

impl<'a> Presentation<'a> {
    /// The fence's info string.
    #[must_use]
    pub fn info(&self) -> &'a str {
        self.info
    }

    /// The fence's own text. **Always present.**
    #[must_use]
    pub fn source(&self) -> &'a str {
        self.source
    }

    /// The enhancement to use instead of rendering the source, if any.
    #[must_use]
    pub fn enhancement(&self) -> Option<&Enhancement> {
        self.enhancement.as_ref()
    }

    /// Why there is no enhancement. `None` exactly when there is one.
    #[must_use]
    pub fn fallback(&self) -> Option<FallbackReason> {
        self.fallback
    }

    fn fell_back(info: &'a str, source: &'a str, why: FallbackReason) -> Self {
        Self {
            info,
            source,
            enhancement: None,
            fallback: Some(why),
        }
    }

    fn enhanced(info: &'a str, source: &'a str, enhancement: Enhancement) -> Self {
        Self {
            info,
            source,
            enhancement: Some(enhancement),
            fallback: None,
        }
    }
}

/// The registered extensions.
#[derive(Default)]
pub struct Registry {
    by_info: BTreeMap<&'static str, &'static dyn Extension>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_info: BTreeMap::new(),
        }
    }

    /// Register `extension` under its own info string.
    #[must_use]
    pub fn with(mut self, extension: &'static dyn Extension) -> Self {
        self.by_info.insert(extension.info(), extension);
        self
    }

    /// Present a fenced block.
    ///
    /// Returns a [`Presentation`] — never a `Result`, and never something
    /// without the source in it. Every refusal below is a fallback carrying a
    /// reason, and none of them is an error.
    pub fn present<'a>(
        &self,
        info: &'a str,
        source: &'a str,
        caps: Capabilities,
        watch: &dyn Stopwatch,
    ) -> Presentation<'a> {
        let key = info.split_whitespace().next().unwrap_or("");
        let lowered = key.to_ascii_lowercase();
        let Some(extension) = self.by_info.get(lowered.as_str()).copied() else {
            return Presentation::fell_back(info, source, FallbackReason::Unregistered);
        };
        if caps.highest <= SupportLevel::Source {
            return Presentation::fell_back(info, source, FallbackReason::SurfaceCannot);
        }
        let budgets = extension.budgets();

        // Budgets BEFORE the handler. A source over any input bound never
        // reaches a renderer, so a registrant cannot forget to check.
        if source.len() > budgets.source_bytes {
            return Presentation::fell_back(
                info,
                source,
                FallbackReason::OverBudget(Budget::SourceBytes),
            );
        }
        let shape = extension.measure(source);
        for (over, which) in [
            (shape.nodes > budgets.nodes, Budget::Nodes),
            (shape.edges > budgets.edges, Budget::Edges),
            (shape.depth > budgets.depth, Budget::Depth),
        ] {
            if over {
                return Presentation::fell_back(info, source, FallbackReason::OverBudget(which));
            }
        }

        let started = watch.read_nanos();
        let produced = extension.render(source, caps.highest);
        let took = watch.read_nanos().saturating_sub(started);

        let Some(enhancement) = produced else {
            return Presentation::fell_back(info, source, FallbackReason::HandlerDeclined);
        };
        // Output bounds are checked on the way OUT, and a breach DISCARDS the
        // whole result. Truncating would emit half a diagram, which is a
        // degraded render — the exact thing this contract exists to prevent.
        if enhancement.payload().len() > budgets.output_bytes {
            return Presentation::fell_back(
                info,
                source,
                FallbackReason::ResultRefused(Budget::OutputBytes),
            );
        }
        if took > budgets.time_nanos {
            return Presentation::fell_back(
                info,
                source,
                FallbackReason::ResultRefused(Budget::Time),
            );
        }
        if enhancement.level() > caps.highest {
            return Presentation::fell_back(info, source, FallbackReason::SurfaceCannot);
        }
        Presentation::enhanced(info, source, enhancement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A stopwatch that returns exactly what a test scripted, so a time budget
    /// is exercised without asserting a real elapsed duration. This box
    /// saturates; a test that measured wall-clock would fail under load and
    /// teach everyone to re-run it.
    struct Scripted {
        readings: Cell<usize>,
        values: Vec<u128>,
    }

    impl Scripted {
        fn new(values: &[u128]) -> Self {
            Self {
                readings: Cell::new(0),
                values: values.to_vec(),
            }
        }
    }

    impl Stopwatch for Scripted {
        fn read_nanos(&self) -> u128 {
            let i = self.readings.get();
            self.readings.set(i + 1);
            self.values.get(i).copied().unwrap_or(0)
        }
    }

    /// Never advances, so every non-time budget is tested without the time
    /// budget being able to fire and mask it.
    struct Frozen;
    impl Stopwatch for Frozen {
        fn read_nanos(&self) -> u128 {
            0
        }
    }

    /// A registrant a test drives: it reports whatever shape it was given and
    /// emits a payload of a chosen size.
    struct Probe {
        shape: Shape,
        payload_len: usize,
        declines: bool,
        budgets: Budgets,
        level: SupportLevel,
    }

    const ROOMY: Budgets = Budgets {
        source_bytes: 1024,
        nodes: 100,
        edges: 100,
        depth: 10,
        output_bytes: 1024,
        time_nanos: 1_000,
    };

    impl Default for Probe {
        fn default() -> Self {
            Self {
                shape: Shape::default(),
                payload_len: 8,
                declines: false,
                budgets: ROOMY,
                level: SupportLevel::Graphics,
            }
        }
    }

    impl Extension for Probe {
        fn info(&self) -> &'static str {
            "probe"
        }
        fn budgets(&self) -> Budgets {
            self.budgets
        }
        fn measure(&self, _source: &str) -> Shape {
            self.shape
        }
        fn render(&self, _source: &str, _level: SupportLevel) -> Option<Enhancement> {
            if self.declines {
                return None;
            }
            let payload = "x".repeat(self.payload_len);
            match self.level {
                SupportLevel::Graphics => Enhancement::graphics(payload, "alt"),
                SupportLevel::Text => Enhancement::text(payload, "alt"),
                SupportLevel::Source => None,
            }
        }
    }

    /// `Registry::with` takes `&'static dyn Extension`, so a test registrant
    /// is leaked deliberately — a test process is the right lifetime for it.
    fn registry_of(probe: Probe) -> Registry {
        Registry::new().with(Box::leak(Box::new(probe)))
    }

    fn graphics() -> Capabilities {
        Capabilities {
            highest: SupportLevel::Graphics,
        }
    }

    // ── the one law ──────────────────────────────────────────────────────

    /// **The source survives every path there is.**
    ///
    /// Not "the fallback works" — that would be a test of one branch. This
    /// enumerates every way a presentation can fail to be enhanced and
    /// requires the source out of all of them, because the contract is that
    /// source is a FIELD and not a branch.
    #[test]
    fn source_is_never_lost_on_any_path() {
        const SRC: &str = "graph TD\n  A --> B";
        let cases: Vec<(&str, Presentation<'_>)> = vec![
            (
                "unregistered info string",
                Registry::new().present("nosuch", SRC, graphics(), &Frozen),
            ),
            (
                "surface cannot enhance",
                registry_of(Probe::default()).present(
                    "probe",
                    SRC,
                    Capabilities::source_only(),
                    &Frozen,
                ),
            ),
            (
                "handler declined",
                registry_of(Probe {
                    declines: true,
                    ..Probe::default()
                })
                .present("probe", SRC, graphics(), &Frozen),
            ),
            (
                "input over budget",
                registry_of(Probe {
                    shape: Shape {
                        nodes: 1_000,
                        ..Shape::default()
                    },
                    ..Probe::default()
                })
                .present("probe", SRC, graphics(), &Frozen),
            ),
            (
                "output over budget",
                registry_of(Probe {
                    payload_len: 99_999,
                    ..Probe::default()
                })
                .present("probe", SRC, graphics(), &Frozen),
            ),
            (
                "over the time budget",
                registry_of(Probe::default()).present(
                    "probe",
                    SRC,
                    graphics(),
                    &Scripted::new(&[0, 999_999]),
                ),
            ),
        ];
        for (what, presentation) in cases {
            assert_eq!(presentation.source(), SRC, "[{what}] source was lost");
            assert!(
                presentation.enhancement().is_none(),
                "[{what}] should not have enhanced"
            );
            assert!(
                presentation.fallback().is_some(),
                "[{what}] a fallback must say why"
            );
        }
    }

    /// **Anti-vacuous twin.** The sibling is satisfied by a registry that
    /// enhances NOTHING, ever. This proves the enhanced path exists and that
    /// the source is kept even then — so "source is never lost" is a claim
    /// about all paths and not about a broken one.
    #[test]
    fn an_enhanced_presentation_still_carries_its_source() {
        const SRC: &str = "graph TD";
        let p = registry_of(Probe::default()).present("probe", SRC, graphics(), &Frozen);
        let enhancement = p.enhancement().expect("this registrant does enhance");
        assert_eq!(enhancement.level(), SupportLevel::Graphics);
        assert_eq!(p.fallback(), None, "an enhanced presentation has no reason");
        assert_eq!(
            p.source(),
            SRC,
            "the source rides along even when it is not what gets drawn"
        );
    }

    /// A handler cannot say "something, but badly": the constructors refuse.
    #[test]
    fn a_malformed_enhancement_is_unrepresentable() {
        assert!(Enhancement::graphics("", "alt").is_none(), "empty payload");
        assert!(
            Enhancement::graphics("   ", "alt").is_none(),
            "blank payload"
        );
        assert!(
            Enhancement::graphics("<svg/>", "").is_none(),
            "graphics with no accessible text is unreadable to a screen reader"
        );
        assert!(Enhancement::text("", "alt").is_none());
        assert!(Enhancement::text("t", "  ").is_none());
        // …and the well-formed one is accepted, or the above passes vacuously.
        assert!(Enhancement::graphics("<svg/>", "a flowchart").is_some());
    }

    // ── budgets, each at its own boundary ────────────────────────────────

    /// Every budget is enforced INDEPENDENTLY, and each is proved by a pair:
    /// exactly at the limit enhances, one over falls back naming that bound.
    /// The pair is the anti-vacuity — remove the budget and the "one over"
    /// case enhances, which fails.
    #[test]
    fn each_budget_is_enforced_at_its_own_boundary() {
        let at_limit = Budgets {
            source_bytes: 4,
            nodes: 2,
            edges: 3,
            depth: 1,
            output_bytes: 5,
            time_nanos: 10,
        };
        let probe = |shape: Shape, payload_len: usize| Probe {
            shape,
            payload_len,
            budgets: at_limit,
            ..Probe::default()
        };
        let full = Shape {
            nodes: 2,
            edges: 3,
            depth: 1,
        };

        // At the limit on every axis at once: enhanced.
        let p = registry_of(probe(full, 5)).present("probe", "1234", graphics(), &Frozen);
        assert!(
            p.enhancement().is_some(),
            "exactly at every limit must still render: {:?}",
            p.fallback()
        );

        // One over, one axis at a time.
        let over: [(&str, Shape, usize, &str, Budget); 5] = [
            ("source bytes", full, 5, "12345", Budget::SourceBytes),
            (
                "nodes",
                Shape { nodes: 3, ..full },
                5,
                "1234",
                Budget::Nodes,
            ),
            (
                "edges",
                Shape { edges: 4, ..full },
                5,
                "1234",
                Budget::Edges,
            ),
            (
                "depth",
                Shape { depth: 2, ..full },
                5,
                "1234",
                Budget::Depth,
            ),
            ("output bytes", full, 6, "1234", Budget::OutputBytes),
        ];
        for (what, shape, payload_len, source, expected) in over {
            let p = registry_of(probe(shape, payload_len)).present(
                "probe",
                source,
                graphics(),
                &Frozen,
            );
            assert!(
                p.enhancement().is_none(),
                "[{what}] one over the limit must not render"
            );
            let expected_reason = if matches!(expected, Budget::OutputBytes) {
                FallbackReason::ResultRefused(expected)
            } else {
                FallbackReason::OverBudget(expected)
            };
            assert_eq!(
                p.fallback(),
                Some(expected_reason),
                "[{what}] must name the bound that fired"
            );
        }

        // …and the time budget, on the injected stopwatch only.
        let p = registry_of(probe(full, 5)).present(
            "probe",
            "1234",
            graphics(),
            &Scripted::new(&[100, 110]),
        );
        assert!(
            p.enhancement().is_some(),
            "10ns elapsed is exactly the limit"
        );
        let p = registry_of(probe(full, 5)).present(
            "probe",
            "1234",
            graphics(),
            &Scripted::new(&[100, 111]),
        );
        assert_eq!(
            p.fallback(),
            Some(FallbackReason::ResultRefused(Budget::Time)),
            "one nanosecond over must refuse, and say it was time"
        );
    }

    /// A budget refusal never reaches the handler — that is what makes the
    /// registry the enforcer rather than each registrant.
    #[test]
    fn an_over_budget_source_never_reaches_the_handler() {
        struct Exploding;
        impl Extension for Exploding {
            fn info(&self) -> &'static str {
                "probe"
            }
            fn budgets(&self) -> Budgets {
                Budgets {
                    source_bytes: 2,
                    ..ROOMY
                }
            }
            fn measure(&self, _: &str) -> Shape {
                Shape::default()
            }
            fn render(&self, _: &str, _: SupportLevel) -> Option<Enhancement> {
                panic!("the registry must refuse before the handler runs");
            }
        }
        let p =
            Registry::new()
                .with(&Exploding)
                .present("probe", "far too long", graphics(), &Frozen);
        assert_eq!(
            p.fallback(),
            Some(FallbackReason::OverBudget(Budget::SourceBytes))
        );
    }

    /// The info string is matched on its first token, case-insensitively —
    /// the same rule the web fence interception has always used.
    #[test]
    fn the_info_string_matches_its_first_token_case_insensitively() {
        let reg = registry_of(Probe::default());
        for info in ["probe", "PROBE", "Probe extra args"] {
            assert!(
                reg.present(info, "s", graphics(), &Frozen)
                    .enhancement()
                    .is_some(),
                "{info:?} should match"
            );
        }
        for info in ["probes", "notprobe", ""] {
            assert_eq!(
                reg.present(info, "s", graphics(), &Frozen).fallback(),
                Some(FallbackReason::Unregistered),
                "{info:?} must not match"
            );
        }
    }

    /// A surface is never handed something richer than it declared.
    #[test]
    fn an_enhancement_above_the_surfaces_level_is_refused() {
        let reg = registry_of(Probe {
            level: SupportLevel::Graphics,
            ..Probe::default()
        });
        let p = reg.present(
            "probe",
            "s",
            Capabilities {
                highest: SupportLevel::Text,
            },
            &Frozen,
        );
        assert!(p.enhancement().is_none(), "graphics to a text-only surface");
        assert_eq!(p.fallback(), Some(FallbackReason::SurfaceCannot));
    }
}
