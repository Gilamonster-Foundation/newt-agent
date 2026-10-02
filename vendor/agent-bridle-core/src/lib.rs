//! `agent-bridle-core` — the capability-enforcement core.
//!
//! This crate is the **leash**: it owns the [`Tool`] trait, the [`Gate`] (the
//! single mint site for a [`ToolContext`]), the [`Registry`], the [`Sandbox`]
//! plumbing, and the result [`ToolEnvelope`]. It re-exports the canonical
//! authority types ([`Caveats`], [`Scope`], [`CountBound`]) from
//! `agent-mesh-protocol` so every host and tool speaks one lattice.
//!
//! The non-bypassable invariant (DESIGN §2): a [`Tool`] can only act through a
//! [`ToolContext`], and a `ToolContext` can only be minted inside
//! [`Gate::authorize`]. So the only path to running a tool runs through the
//! leash, and the tool receives the *meet* of granted-and-required authority —
//! least authority by construction.
//!
//! Dependency budget is deliberately tiny — `anyhow`, `serde`, `serde_json`,
//! `async-trait`, `agent-mesh-protocol`. No tokio. No brush. Heavy runtimes
//! live in leaf tool crates only. The [`step_up`] module's content-addressing
//! reuses `agent-mesh-protocol`'s BLAKE3 primitive (no new runtime dep); its
//! production `Ed25519Verifier` is gated behind the off-by-default
//! `verifier-ed25519` feature (pulls `ed25519-dalek`), so the default build
//! stays lean.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

// ── The leash: canonical authority lattice (re-exported, single source) ──────
pub use agent_mesh_protocol::{Caveats, CountBound, Scope};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod delegated_control;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use delegated_control::{receive_control_endpoint, send_control_endpoint};

mod config;
mod context;
mod envelope;
mod error;
mod execution;
mod gate;
// The loopback egress proxy (#124/#257, ADR 0016) — moved here from
// agent-bridle-tool-shell so BOTH the shell engine and external long-lived
// callers (a confined MCP subprocess via `ConfinedCommand::spawn_tokio`, or a
// no-subprocess `reqwest` client) share ONE implementation. Std-only; public so
// the audit seams (`AuditSink`/`Resolver`) stay embedder-extensible.
pub mod net_proxy;
// The operator-authz contract (newt-agent #1354): HumanPrincipal /
// PrincipalBinding / PermissionChallenge / OperatorDecision. Public like
// `policy` so consumers reach it as `agent_bridle::operator::…`.
mod admitted;
pub mod operator;
pub mod policy;
mod provenance;
mod registry;
mod report;
#[cfg(target_os = "linux")]
mod rootfs;
mod sandbox;
mod spawn;
mod step_up;
mod tool;
mod unbridle;

pub use admitted::{
    AdmittedFence, AdmittedFenceBody, AdmittedFenceId, BackendProjection, RuntimeClosure,
};
pub use config::{
    default_env_denylist, default_exec_path, fence_env, BackendToggles, BridleConfig, BridleMode,
    ChildNetworkPolicy, GatePolicy, HostMatch, LimitsPolicy, NetDefault, NetPolicy, NetRule,
    NormalizationPolicy, PathList, RootfsPolicy, SandboxPolicy, VmPolicy, WebPolicy,
};
pub use context::{open_scoped_read, open_scoped_write, ToolContext};
pub use envelope::{Denial, DenialKind, Disclosure, ToolEnvelope};
pub use error::{ToolError, ToolResult};
pub use execution::{
    execution_stream, local_tree_containment, DroppedEvidence, ExecutionControl, ExecutionEmit,
    ExecutionEvent, ExecutionEventKind, ExecutionEventSink, ExecutionHandle, ExecutionId,
    ExecutionLimits, ExecutionRequest, ExecutionStdin, ExecutionTerminal, ExitDisposition,
    ExitEvidence, FenceEvidence, LocalExecutionBackend, LocalTreeContainment, OutputStream,
    MAX_ARGV_ENTRIES, MAX_ARG_BYTES, MAX_ENV_ENTRIES, MAX_OUTPUT_CHUNK_BYTES_CEILING,
    MAX_QUEUED_EVENTS_CEILING, MAX_QUEUED_OUTPUT_BYTES_CEILING, MAX_STDIN_BYTES_CEILING,
};
pub use gate::Gate;
pub use net_proxy::{start_egress_proxy, ProxyFinalEvidence, ProxyFinalizeError, ProxyHandle};
pub use provenance::{
    admit, empty_closure, relate, AdmissionDecision, AdmissionReject, ConfinedAxis,
    ResolvedAuthority, ResolvedScope, ScopeRelation,
};
pub use registry::{Grant, Registry, RegistryBuilder};
pub use report::{
    enforcement_report, fence_strength, unenforceable_axis, AxisEnforcement, ConfinementMechanism,
    EnforcementFloor, EnforcementReport, ExecBoundary, UnenforceableAxis,
};
#[cfg(target_os = "linux")]
pub use rootfs::{build_rootfs_plan, materialize_copy, RootfsCache, RootfsEntry, RootfsPlan};
pub use sandbox::{
    best_available_sandbox, effective_sandbox_kind, egress_proxy_plan, loopback_fenced_caveats,
    net_egress_proxy_hosts, HeldReadRoot, NoopSandbox, Sandbox, SandboxKind,
};
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
pub use sandbox::{landlock_is_supported, landlock_net_is_supported, LandlockSandbox};
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
pub use sandbox::{seatbelt_is_supported, SeatbeltSandbox};
pub use spawn::{
    confinement_unenforceable, decode_trusted_worker_frame_header, decode_trusted_worker_hello,
    decode_trusted_worker_request, encode_trusted_worker_frame_header, encode_trusted_worker_hello,
    spawn_confined_subprocess, trusted_worker_frame_digest, ConfinedChild, ConfinedCommand,
    ManagedSpawn, SandboxedWorker, SandboxedWorkerChild, TrustedWorkerKind, TrustedWorkerRequest,
    TRUSTED_WORKER_ACK, TRUSTED_WORKER_BOOTSTRAP, TRUSTED_WORKER_FRAME_HEADER_LEN,
    TRUSTED_WORKER_HELLO_LEN, TRUSTED_WORKER_MAX_BODY, TRUSTED_WORKER_PROTOCOL_VERSION,
};
// The async-host confined spawn (tokio pipe handles). Unix-only, feature-gated,
// so it re-exports only when built — mirroring the OS-sandbox re-exports above.
#[cfg(all(unix, feature = "spawn-tokio"))]
pub use spawn::ConfinedTokioChild;
#[cfg(feature = "verifier-ed25519")]
pub use step_up::Ed25519Verifier;
#[cfg(feature = "verifier-webauthn-es256")]
pub use step_up::WebAuthnEs256Verifier;
#[cfg(feature = "verifier-webauthn")]
pub use step_up::WebAuthnVerifier;
pub use step_up::{
    AttestRequirement, Attestation, CallRequest, Challenge, ContentId, Decision, Discharge,
    DischargeAttempt, DischargeProvider, DischargeVerifier, Presence, Rule, SessionId,
    StepUpPolicy,
};
pub use tool::{Invocation, Tool};
pub use unbridle::{human_gate, is_unbridled, set_human_gate, set_unbridled, HumanGate};

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial tool used to drive the gate in crate-level tests.
    struct T;
    #[async_trait::async_trait]
    impl Tool for T {
        fn name(&self) -> &str {
            "t"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn invoke(
            &self,
            _args: serde_json::Value,
            _cx: &ToolContext,
        ) -> ToolResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
    }

    /// The mint-token is un-constructible outside this crate.
    ///
    /// `ToolContext` exposes no public constructor and no public fields. The
    /// `compile_fail` doctest below proves a downstream crate cannot forge one;
    /// here we assert the *only* way to get one is through the gate, and that
    /// what it carries is the meet (≤ granted).
    #[test]
    fn context_minted_only_by_gate_and_carries_meet() {
        let granted = Caveats {
            exec: Scope::only(["echo".to_string()]),
            max_calls: CountBound::AtMost(3),
            ..Caveats::top()
        };
        let gate = Gate::new(0);
        let cx = gate.authorize(&T, &granted).expect("authorize");
        // effective ⊑ granted (least authority).
        assert!(cx.caveats().leq(&granted));
        // The default `required()` is top, so effective == granted here.
        assert_eq!(*cx.caveats(), granted);
    }

    /// `ToolContext` cannot be constructed outside `agent-bridle-core`: it has
    /// no public constructor and no public fields, so a downstream caller has
    /// no syntax to make one. (Doctest crates are treated as external, so this
    /// proves the cross-crate boundary.)
    ///
    /// ```compile_fail
    /// use agent_bridle_core::{AxisEnforcement, ToolContext};
    /// // No public constructor:
    /// let _ = ToolContext::mint(agent_bridle_core::Caveats::top(),
    ///     agent_bridle_core::SandboxKind::None, AxisEnforcement::Advisory);
    /// ```
    ///
    /// ```compile_fail
    /// use agent_bridle_core::{AxisEnforcement, Caveats, SandboxKind, ToolContext};
    /// // No public fields, no struct literal possible:
    /// let _ = ToolContext {
    ///     effective: Caveats::top(),
    ///     sandbox_kind: SandboxKind::None,
    ///     strength_floor: AxisEnforcement::Advisory,
    /// };
    /// ```
    fn _mint_token_is_unconstructible_doctests() {}
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
