//! SEED for the **Windows agent** (runs on real Windows CI). Cross-platform OCAP
//! closure — the AppContainer half of the theorem:
//!
//! > For every supported Newt attacker-exec route, on Windows, either the
//! > requested authority is enforced by a real OS boundary (AppContainer / LPAC /
//! > restricted token + ACL-scoped workspace) with adversarial evidence, or
//! > execution refuses fail-closed before hostile code runs.
//!
//! This file is `cfg`'d to Windows + `windows-appcontainer`, so it is INERT on
//! Linux (compiles to nothing) — the Linux maintainer can push the skeleton
//! without breaking Linux CI. On Windows CI it runs the real adversarial suite.
//!
//! Trace the ACTUAL launcher (agent-bridle-aclaunch / restricted token). Do NOT
//! infer protection from the existence of an "aclaunch" binary.
//!
//! RULES (from the review — do not soften):
//! * Real-resource only. Compiling ≠ evidence. Distinguish DENIED-BY-KERNEL/OS
//!   POLICY from COMMAND-HAPPENED-NOT-TO-WORK — only the former is evidence.
//! * Prove the sandbox follows the PROCESS TREE (child → cmd/PowerShell/helper →
//!   grandchild), reporting the effective token/AppContainer identity ≥2 deep.
//! * Timeout/cancellation cleanup is a SEPARATE property from authority
//!   containment — keep it distinct, do not smuggle it into a CLOSED assertion.
//! * A missing/unavailable AppContainer backend must REFUSE (fail-closed), never
//!   fall back to an ordinary host cmd/PowerShell. (See the ACTIVE
//!   `unconfined-fallback-on-missing-backend` deviation.)
//! * Windows named-pipe/local-IPC deputy is the analog of the Linux AF_UNIX
//!   finding — do NOT assume AppContainer's direct-network restrictions cover
//!   ambient IPC deputies.
//!
//! Deliverable: implement every `#[ignore]` below, add run_command lib tests
//! (pub(crate) `dispatch_bridled_shell` — mirror the Linux lib tests in
//! `tools.rs`), fill `docs/security/platform/windows-evidence.md`, and update
//! the register's platform-scoped states from the EVIDENCE.
#![cfg(all(target_os = "windows", feature = "windows-appcontainer"))]

// TODO(windows-agent): import the confined-exec surface + the aclaunch/token API.

// ── Filesystem authority (ACL-scoped workspace) ─────────────────────────────
#[test]
#[ignore = "TODO(windows-agent): read a secret elsewhere in the user's profile → DENIED by AppContainer/ACL"]
fn appcontainer_denies_profile_secret_read() {}

#[test]
#[ignore = "TODO(windows-agent): write OUTSIDE the workspace → DENIED"]
fn appcontainer_denies_outside_workspace_write() {}

#[test]
#[ignore = "TODO(windows-agent): modify a sibling directory → DENIED"]
fn appcontainer_denies_sibling_dir_write() {}

#[test]
#[ignore = "TODO(windows-agent): escape via junction / symlink / reparse point / UNC path / alternate path spelling → DENIED"]
fn appcontainer_denies_reparse_and_unc_escape() {}

// ── Environment / credential inheritance ────────────────────────────────────
#[test]
#[ignore = "TODO(windows-agent): parent/provider credential env vars must be ABSENT in the child"]
fn appcontainer_child_does_not_inherit_provider_credentials() {}

// ── Direct network ──────────────────────────────────────────────────────────
#[test]
#[ignore = "TODO(windows-agent): outbound TCP under net:none → DENIED (AppContainer network capability absent)"]
fn appcontainer_denies_direct_tcp() {}

#[test]
#[ignore = "TODO(windows-agent): outbound UDP under net:none → DENIED"]
fn appcontainer_denies_direct_udp() {}

#[test]
#[ignore = "TODO(windows-agent): loopback under net:none → record DENIED vs allowed (AppContainer loopback capability)"]
fn appcontainer_loopback_behavior() {}

// ── Local-deputy egress (named pipe = the Linux AF_UNIX analog) ──────────────
#[test]
#[ignore = "TODO(windows-agent): host deputy on a named pipe; confined child connects + the deputy relays network. \
            DENIED → regression test; REACHABLE → register a Windows local-deputy residual."]
fn appcontainer_named_pipe_deputy() {}

// ── Handle hygiene ──────────────────────────────────────────────────────────
#[test]
#[ignore = "TODO(windows-agent): a deliberately INHERITABLE HANDLE — is it inherited by the child? (bInheritHandles / explicit allowlist)"]
fn appcontainer_inheritable_handle_inheritance() {}

// ── Process-tree containment (≥2 generations) ───────────────────────────────
#[test]
#[ignore = "TODO(windows-agent): child → cmd.exe → grandchild; report the effective token/AppContainer identity — restriction must hold ≥2 deep"]
fn appcontainer_descendants_stay_in_the_same_token() {}

#[test]
#[ignore = "TODO(windows-agent): invoke cmd.exe, PowerShell, a workspace .exe, git — the boundary must follow the process tree, not the initial exe"]
fn appcontainer_follows_shells_and_helpers() {}

// ── Timeout/cancellation (SEPARATE residual from authority) ──────────────────
#[test]
#[ignore = "TODO(windows-agent): timed-out child (and its descendants) are killed — a Job Object property, tracked SEPARATELY from authority containment"]
fn appcontainer_timeout_cleanup_is_distinct_from_authority() {}

// ── Fail-closed / no silent host fallback ───────────────────────────────────
#[test]
#[ignore = "TODO(windows-agent): force AppContainer unavailable; a RESTRICTED-axis AgentInfluenced spawn must REFUSE, never run ordinary cmd/PowerShell"]
fn appcontainer_missing_backend_refuses_not_host() {}
