//! The result envelope — the MCP-shaped JSON a tool returns.
//!
//! Tools that run a subprocess-like operation (the shell, exec-style tools)
//! return a uniform shape so a frontend can render them identically and so the
//! recorded [`crate::SandboxKind`] travels with every result (DESIGN §6: "the
//! Gate records `sandbox_kind` in **every** `ToolResult`").

use crate::{EnforcementReport, HumanGate, SandboxKind};

mod execution;

/// Which kind of capability operation the leash refused.
///
/// Mirrors the brush `CommandInterceptor` hooks: an `exec` denial comes from
/// `before_exec` (an out-of-scope program, including a path-separator-spelled
/// one like `/bin/rm`), an `open` denial from `before_open` (a redirection or
/// `source` target outside `fs_read`/`fs_write`). A `net` denial comes from the
/// loopback egress proxy refusing an out-of-allow-list host (#196) — the one
/// axis observed *during* the run rather than at pre-spawn admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialKind {
    /// An external command spawn was denied (`before_exec`).
    Exec,
    /// A file open (redirection/`source`) was denied (`before_open`).
    Open,
    /// A network egress to an out-of-allow-list host was refused by the egress
    /// proxy (#196). The [`Denial::target`] is the CONNECT host, so a consumer
    /// can prompt per-host. Only observable where a proxy is in the data path
    /// (a non-empty `net` allow-list); a pure deny-all is kernel-fenced and
    /// surfaces no host.
    Net,
}

/// One structured leash denial recorded by the in-process interceptor.
///
/// This is the **structured security signal** that replaces stderr
/// string-matching: a denial is present here *only* when the interceptor
/// actually decided `Deny`, never merely because a permitted command exited
/// non-zero on its own.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Denial {
    /// Whether an `exec` (spawn), an `open` (file), or a `net` egress was refused.
    pub kind: DenialKind,
    /// The exact target the interceptor saw: the program (e.g. `rm`,
    /// `/bin/rm`) for an `exec` denial, the path for an `open` denial, or the
    /// CONNECT host (e.g. `github.com`) for a `net` denial (#196).
    pub target: String,
    /// The human-readable reason from the leash (safe to surface to an agent).
    pub reason: String,
}

/// Operator-facing **disclosure** — what an operator *should know* about how this
/// run was shaped, kept **strictly separate** from the [`EnforcementReport`]
/// (ADR 0016 precedent / ADR 0017 D6). Disclosure is informational: it records
/// over-delivery, disabled normalizations, a forced backend, and the loud
/// `unbridled` opt-in. It **never** participates in [`crate::fence_strength`] or
/// the enforcement claim — a run can never *raise* its confinement claim by
/// disclosing something, and disclosing something can never *lower* it either.
///
/// Quiet by default: the whole block is omitted from JSON when nothing is worth
/// disclosing (the common bridled path). The one field that always surfaces when
/// set is [`Self::unbridled`] — an unbridled run is never quietly hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Disclosure {
    /// The run was explicitly **unbridled** (confinement off — `Caveats::top()` +
    /// advisory floor + `SandboxKind::None`), an acknowledged operator opt-in
    /// (#151/I12). Always emitted when `true`; never reachable by omission.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unbridled: bool,
    /// Automatic normalizations the operator turned off, by name (e.g.
    /// `ldd_closure`, `nss_closure_fallback`) — so a degraded run is legible.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub normalizations_disabled: Vec<String>,
    /// A restricted `net` allow-list is enforced **above** the reported floor —
    /// the loopback egress proxy admits exactly the granted hosts while the report
    /// honestly keeps the axis `advisory` (proxy-, not kernel-, enforced; #124/#128,
    /// ADR 0016). Discloses the over-delivery without raising the claim.
    #[serde(default, skip_serializing_if = "is_false")]
    pub net_over_delivery: bool,
    /// A sandbox backend was overridden from the default selection (downgrade /
    /// select-available only; #149/I10). Names the backend actually applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_forced: Option<String>,
    /// Which shell **engine** ran this operation (ADR 0019 D4 / #194) — e.g.
    /// `"safe-subset"` or `"sandbox-host"`. Lets an embedder log which engine a
    /// dispatch used when more than one is registered. `None` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// The human step-up gate still in force (ADR 0018 D11 / R5). Distinguishes an
    /// unbridled run's two postures — `passkey`/`prompt` = *Supervised-free* (a
    /// gesture still gates HIGH-consequence acts), `none` = *Autonomous* (no human
    /// in the loop). Shown whenever the block is emitted (i.e. when unbridled), so
    /// a consumer can never confuse "free but FIDO-gated" with "no human at all".
    pub human_gate: HumanGate,
}

impl Disclosure {
    /// `true` when there is nothing to disclose — the whole block is then omitted
    /// from JSON. (An `unbridled` run is *not* quiet, so it always surfaces.)
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        !self.unbridled
            && self.normalizations_disabled.is_empty()
            && !self.net_over_delivery
            && self.backend_forced.is_none()
            && self.engine.is_none()
    }
}

/// A structured execution result. Serialized to the MCP content shape via
/// [`ToolEnvelope::into_json`]; absent fields are omitted.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolEnvelope {
    /// Process exit code, when the operation had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Captured standard output, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Captured standard error, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// Whether the operation was cut short by a timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    /// Whether captured stdout was clipped at the output cap (more was produced
    /// than was kept). Lets a consumer tell a complete result from a truncated
    /// one. Omitted (treated as `false`) when output was not clipped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stdout_truncated: bool,
    /// Whether captured stderr was clipped at the output cap. Omitted when not.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stderr_truncated: bool,
    /// Whether the in-process leash recorded at least one denial during this
    /// invocation. This is a **structured** signal: it is set iff
    /// [`Self::denials`] is non-empty, so a consumer never has to string-match
    /// stderr to detect a security refusal. Omitted (treated as `false`) when
    /// no denial was recorded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub denied: bool,
    /// The denials the interceptor recorded, in the order they occurred. Empty
    /// (and omitted from JSON) unless [`Self::denied`] is `true`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub denials: Vec<Denial>,
    /// The OS-level sandbox in force when this ran. Always present so callers
    /// can tell whether the leash was kernel-enforced or advisory.
    pub sandbox_kind: SandboxKind,
    /// Per-axis confinement report (ADR 0004 D1): for each **restricted** axis,
    /// whether it is `kernel` / `interceptor` / `advisory`. Refines the coarse
    /// `sandbox_kind` (which stays the *minimum* claim) at axis grain. Omitted
    /// from JSON when no axis is restricted.
    #[serde(skip_serializing_if = "EnforcementReport::is_empty", default)]
    pub enforcement: EnforcementReport,
    /// Operator-facing disclosure (ADR 0017 D6) — informational only, **never**
    /// part of the enforcement claim. Quiet by default (omitted when nothing is
    /// worth disclosing); an `unbridled` run always surfaces here.
    #[serde(skip_serializing_if = "Disclosure::is_quiet", default)]
    pub disclosure: Disclosure,
    /// The actual Started event, when a managed root acquired a process tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_started: Option<crate::ExecutionEvent>,
    /// The actual managed terminal event, including failure or pre-spawn denial.
    /// Together with `execution_started` this is a bounded pair, not an output log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<crate::ExecutionEvent>,
}

/// `skip_serializing_if` helper: omit `denied` from JSON when it is `false`.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl ToolEnvelope {
    /// An envelope stamped with the sandbox kind and nothing else set.
    #[must_use]
    pub fn new(sandbox_kind: SandboxKind) -> Self {
        Self {
            sandbox_kind,
            ..Self::default()
        }
    }

    /// Set the exit code (builder style).
    #[must_use]
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }

    /// Set captured stdout (builder style).
    #[must_use]
    pub fn with_stdout(mut self, stdout: impl Into<String>) -> Self {
        self.stdout = Some(stdout.into());
        self
    }

    /// Set captured stderr (builder style).
    #[must_use]
    pub fn with_stderr(mut self, stderr: impl Into<String>) -> Self {
        self.stderr = Some(stderr.into());
        self
    }

    /// Mark whether the operation timed out (builder style).
    #[must_use]
    pub fn with_timed_out(mut self, timed_out: bool) -> Self {
        self.timed_out = Some(timed_out);
        self
    }

    /// Mark whether captured stdout/stderr were clipped at the cap (builder
    /// style). A truncated stream is a *bounded* read: peak buffering never
    /// exceeds the cap regardless of how much the child produced.
    #[must_use]
    pub fn with_truncation(mut self, stdout_truncated: bool, stderr_truncated: bool) -> Self {
        self.stdout_truncated = stdout_truncated;
        self.stderr_truncated = stderr_truncated;
        self
    }

    /// Attach the leash denials the interceptor recorded (builder style).
    ///
    /// [`Self::denied`] is set to `true` iff `denials` is non-empty, so the
    /// boolean flag and the list can never disagree. Passing an empty vec is a
    /// no-op (the result stays un-denied), which keeps the common
    /// nothing-was-denied path clean.
    #[must_use]
    pub fn with_denials(mut self, denials: Vec<Denial>) -> Self {
        self.denied = !denials.is_empty();
        self.denials = denials;
        self
    }

    /// Attach the per-axis confinement report (builder style; ADR 0004 D1).
    #[must_use]
    pub fn with_enforcement(mut self, enforcement: EnforcementReport) -> Self {
        self.enforcement = enforcement;
        self
    }

    /// Attach the operator-facing disclosure (builder style; ADR 0017 D6).
    /// Purely informational — it does not affect [`Self::enforcement`],
    /// [`Self::sandbox_kind`], or any confinement claim.
    #[must_use]
    pub fn with_disclosure(mut self, disclosure: Disclosure) -> Self {
        self.disclosure = disclosure;
        self
    }

    /// Serialize to the JSON content shape tools return.
    ///
    /// # Panics
    /// Never in practice: the envelope contains only JSON-representable scalars.
    #[must_use]
    pub fn into_json(self) -> serde_json::Value {
        serde_json::to_value(self).expect("ToolEnvelope is always JSON-serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omits_absent_fields_keeps_sandbox_kind() {
        let v = ToolEnvelope::new(SandboxKind::None)
            .with_exit_code(0)
            .with_stdout("hi\n")
            .into_json();
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "hi\n");
        assert!(v.get("stderr").is_none());
        assert!(v.get("timed_out").is_none());
        assert_eq!(v["sandbox_kind"], "none");
    }

    #[test]
    fn no_denials_omits_denied_and_denials() {
        // The common case: nothing was denied → neither structured field
        // appears in the JSON, so `denied` defaults to false for consumers.
        let v = ToolEnvelope::new(SandboxKind::None)
            .with_exit_code(0)
            .with_denials(Vec::new())
            .into_json();
        assert!(v.get("denied").is_none(), "denied must be omitted: {v}");
        assert!(v.get("denials").is_none(), "denials must be omitted: {v}");
    }

    #[test]
    fn recorded_denials_set_denied_true_and_list() {
        let denials = vec![Denial {
            kind: DenialKind::Exec,
            target: "rm".to_string(),
            reason: "exec of \"rm\" is not within the granted authority".to_string(),
        }];
        let v = ToolEnvelope::new(SandboxKind::None)
            .with_exit_code(126)
            .with_denials(denials)
            .into_json();
        assert_eq!(v["denied"], true);
        assert_eq!(v["denials"][0]["kind"], "exec");
        assert_eq!(v["denials"][0]["target"], "rm");
        assert!(v["denials"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not within the granted"));
    }

    #[test]
    fn enforcement_report_is_threaded_and_omitted_when_empty() {
        use crate::{enforcement_report, AxisEnforcement, Caveats, Scope};
        // Restricted fs_write under Landlock → the envelope carries a kernel claim.
        let caveats = Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            ..Caveats::top()
        };
        let report = enforcement_report(&caveats, SandboxKind::Landlock);
        assert_eq!(report.fs_write, Some(AxisEnforcement::Kernel));
        let v = ToolEnvelope::new(SandboxKind::Landlock)
            .with_enforcement(report)
            .with_exit_code(0)
            .into_json();
        assert_eq!(v["enforcement"]["fs_write"], "kernel");
        assert!(
            v["enforcement"].get("exec").is_none(),
            "unrestricted axis omitted"
        );

        // An all-`All` grant produces an empty report → the field is omitted.
        let empty = enforcement_report(&Caveats::top(), SandboxKind::None);
        let v2 = ToolEnvelope::new(SandboxKind::None)
            .with_enforcement(empty)
            .into_json();
        assert!(
            v2.get("enforcement").is_none(),
            "empty report omitted: {v2}"
        );
    }

    #[test]
    fn disclosure_is_quiet_by_default_and_omitted() {
        // The common bridled path discloses nothing → the block is absent.
        let v = ToolEnvelope::new(SandboxKind::Landlock)
            .with_exit_code(0)
            .into_json();
        assert!(
            v.get("disclosure").is_none(),
            "a quiet disclosure must be omitted: {v}"
        );
        assert!(Disclosure::default().is_quiet());
    }

    #[test]
    fn unbridled_disclosure_always_surfaces() {
        let v = ToolEnvelope::new(SandboxKind::None)
            .with_disclosure(Disclosure {
                unbridled: true,
                ..Disclosure::default()
            })
            .into_json();
        assert_eq!(
            v["disclosure"]["unbridled"], true,
            "an unbridled run must never be quietly hidden: {v}"
        );
    }

    #[test]
    fn disclosure_fields_surface_when_set() {
        let v = ToolEnvelope::new(SandboxKind::Seatbelt)
            .with_disclosure(Disclosure {
                normalizations_disabled: vec!["ldd_closure".to_string()],
                net_over_delivery: true,
                backend_forced: Some("seatbelt".to_string()),
                ..Disclosure::default()
            })
            .into_json();
        assert_eq!(v["disclosure"]["normalizations_disabled"][0], "ldd_closure");
        assert_eq!(v["disclosure"]["net_over_delivery"], true);
        assert_eq!(v["disclosure"]["backend_forced"], "seatbelt");
        // A quiet sub-field (unbridled=false) stays omitted within the block.
        assert!(v["disclosure"].get("unbridled").is_none());
    }

    #[test]
    fn disclosure_human_gate_distinguishes_postures() {
        // Supervised-free: unbridled but the passkey gate remains.
        let sf = ToolEnvelope::new(SandboxKind::None)
            .with_disclosure(Disclosure {
                unbridled: true,
                human_gate: HumanGate::Passkey,
                ..Disclosure::default()
            })
            .into_json();
        assert_eq!(sf["disclosure"]["human_gate"], "passkey");
        // Autonomous: unbridled AND no human in the loop — must be distinguishable.
        let auto = ToolEnvelope::new(SandboxKind::None)
            .with_disclosure(Disclosure {
                unbridled: true,
                human_gate: HumanGate::None,
                ..Disclosure::default()
            })
            .into_json();
        assert_eq!(auto["disclosure"]["human_gate"], "none");
    }

    #[test]
    fn disclosure_never_affects_the_enforcement_claim() {
        use crate::{enforcement_report, Caveats, Scope};
        // The honesty invariant (ADR 0017 D6): disclosure is informational — it
        // cannot change the sandbox_kind or the enforcement report.
        let caveats = Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            ..Caveats::top()
        };
        let report = enforcement_report(&caveats, SandboxKind::Landlock);
        let bare = ToolEnvelope::new(SandboxKind::Landlock).with_enforcement(report);
        let disclosed = ToolEnvelope::new(SandboxKind::Landlock)
            .with_enforcement(report)
            .with_disclosure(Disclosure {
                unbridled: true,
                net_over_delivery: true,
                ..Disclosure::default()
            });
        assert_eq!(bare.sandbox_kind, disclosed.sandbox_kind);
        assert_eq!(bare.enforcement, disclosed.enforcement);
        let (bv, dv) = (bare.into_json(), disclosed.into_json());
        assert_eq!(bv["sandbox_kind"], dv["sandbox_kind"]);
        assert_eq!(bv["enforcement"], dv["enforcement"]);
    }

    #[test]
    fn denial_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(DenialKind::Exec).unwrap(),
            serde_json::json!("exec")
        );
        assert_eq!(
            serde_json::to_value(DenialKind::Open).unwrap(),
            serde_json::json!("open")
        );
        // #196: the net axis serializes as "net" — the exact string a downstream
        // consumer (newt) matches to lift it into a per-host prompt.
        assert_eq!(
            serde_json::to_value(DenialKind::Net).unwrap(),
            serde_json::json!("net")
        );
        assert_eq!(
            serde_json::from_value::<DenialKind>(serde_json::json!("net")).unwrap(),
            DenialKind::Net
        );
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
