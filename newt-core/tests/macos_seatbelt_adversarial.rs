//! SEED for the **macOS agent** (runs on real macOS CI). Cross-platform OCAP
//! closure — the Seatbelt half of the theorem:
//!
//! > For every supported Newt attacker-exec route, on macOS, either the requested
//! > authority is enforced by a real OS boundary (Seatbelt) with adversarial
//! > evidence, or execution refuses fail-closed before hostile code runs.
//!
//! This file is `cfg`'d to macOS + `macos-seatbelt`, so it is INERT on Linux (it
//! compiles to nothing) — the Linux maintainer can push the skeleton without
//! breaking Linux CI. On macOS CI it runs the real adversarial suite.
//!
//! Mirror the Linux ground-truth pattern (`newt-core/tests/af_unix_deputy.rs`,
//! `net_guard_executor.rs`): drive the PUBLIC `ConstrainedExecutor` route here;
//! add the `run_command` route (`dispatch_bridled_shell`) as LIB tests in
//! `newt-core/src/agentic/tools.rs` (that fn is `pub(crate)`), mirroring
//! `run_command_child_can_reach_an_af_unix_abstract_deputy`.
//!
//! RULES (from the review — do not soften):
//! * Real-resource only. Compiling ≠ evidence. Each test must DENY-by-kernel or
//!   REFUSE-before-exec, never "the command happened not to work".
//! * Distinguish DENIED-BY-SEATBELT from command-not-found. Assert on the
//!   Seatbelt envelope (`sandbox_kind == "seatbelt"`, `enforcement.*`).
//! * Inspect the GENERATED Seatbelt profile and PIN meaningful properties, so a
//!   future profile change cannot silently widen authority.
//! * A missing/unavailable Seatbelt backend must REFUSE (fail-closed), never fall
//!   back to a host shell. (See the ACTIVE `unconfined-fallback-on-missing-backend`
//!   deviation — the run_command route currently does NOT; capture the macOS
//!   truth and file it under the platform-scoped state.)
//! * Where Seatbelt cannot enforce an axis Newt requests, model it UNSUPPORTED /
//!   residual — do NOT pretend Landlock/seccomp equivalence.
//!
//! Deliverable: fill every `#[ignore]` below with a real assertion, add the
//! run_command lib tests, fill the macOS column of the route + adversarial
//! matrices in `docs/security/platform/macos-evidence.md`, and update the
//! deviation register's platform-scoped states from the EVIDENCE.
#![cfg(all(target_os = "macos", feature = "macos-seatbelt"))]

// TODO(macos-agent): import the confined-exec surface (see net_guard_executor.rs):
//   use newt_core::confined_exec::{
//       workspace_confined_caveats, ConstrainedExecutor, ExecOrigin, ExecRequest, ...
//   };
//   use agent_bridle::seatbelt_is_supported; // gate/skip helper

// ── Filesystem authority ────────────────────────────────────────────────────
#[test]
#[ignore = "TODO(macos-agent): implement — hostile child reads a secret OUTSIDE the workspace → DENIED by Seatbelt"]
fn seatbelt_denies_outside_workspace_read() {}

#[test]
#[ignore = "TODO(macos-agent): write OUTSIDE the workspace → DENIED"]
fn seatbelt_denies_outside_workspace_write() {}

#[test]
#[ignore = "TODO(macos-agent): modify a SIBLING repo dir → DENIED"]
fn seatbelt_denies_sibling_repo_write() {}

#[test]
#[ignore = "TODO(macos-agent): symlink / canonicalization escape out of the fence → DENIED"]
fn seatbelt_denies_symlink_escape() {}

// ── Environment / credential inheritance ────────────────────────────────────
#[test]
#[ignore = "TODO(macos-agent): a parent-only secret env var (e.g. NEWT_AGENT_KEY) must be ABSENT in the child"]
fn seatbelt_child_does_not_inherit_parent_credentials() {}

// ── Direct network (separate from local-deputy) ─────────────────────────────
#[test]
#[ignore = "TODO(macos-agent): outbound TCP to a literal IP under net:none → DENIED (assert curl exit / enforcement.net)"]
fn seatbelt_denies_direct_tcp() {}

#[test]
#[ignore = "TODO(macos-agent): outbound UDP under net:none → DENIED"]
fn seatbelt_denies_direct_udp() {}

#[test]
#[ignore = "TODO(macos-agent): loopback connect under net:none → record DENIED vs allowed (Seatbelt loopback semantics)"]
fn seatbelt_loopback_behavior() {}

// ── Local-deputy egress (the Linux AF_UNIX lesson, repeated) ─────────────────
#[test]
#[ignore = "TODO(macos-agent): host deputy on a pathname AF_UNIX socket; confined child connects + relays. \
            DENIED → regression test; REACHABLE → register a macOS local-deputy residual."]
fn seatbelt_pathname_af_unix_deputy() {}

#[test]
#[ignore = "TODO(macos-agent): inspect the effective Seatbelt profile for Mach/XPC lookup privileges that could \
            reach an ambient host service acting as a network/fs deputy; register any reachable one."]
fn seatbelt_mach_xpc_deputy_surface() {}

// ── Descriptor / handle hygiene ─────────────────────────────────────────────
#[test]
#[ignore = "TODO(macos-agent): a deliberately non-CLOEXEC descriptor — is it inherited by the child? \
            (mirror run_command_route_fd_hygiene_is_cloexec_based_not_explicit_close)"]
fn seatbelt_non_cloexec_fd_inheritance() {}

// ── Process-tree containment (the sandbox must follow the tree) ──────────────
#[test]
#[ignore = "TODO(macos-agent): child → grandchild repeat the fs/net attacks; the Seatbelt boundary must hold ≥2 generations deep"]
fn seatbelt_descendants_stay_confined() {}

#[test]
#[ignore = "TODO(macos-agent): invoke /bin/sh, zsh, python3, git — the sandbox must follow the PROCESS TREE, not the original executable"]
fn seatbelt_follows_interpreters_and_helpers() {}

// ── Fail-closed / no silent host fallback ───────────────────────────────────
#[test]
#[ignore = "TODO(macos-agent): force the Seatbelt backend unavailable (backends.disable=[\"seatbelt\"]); a RESTRICTED-axis \
            AgentInfluenced spawn must REFUSE (confinement_unenforceable), never run on the host"]
fn seatbelt_missing_backend_refuses_not_host() {}

// ── Profile pinning (a profile change cannot silently widen authority) ───────
#[test]
#[ignore = "TODO(macos-agent): render the generated sandbox-exec profile for a representative confined request and \
            assert the meaningful (deny network*) / file-read/write scoping clauses are present"]
fn seatbelt_generated_profile_pins_the_boundary() {}
