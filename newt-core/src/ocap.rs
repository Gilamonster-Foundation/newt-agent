//! OCAP enforcement scaffold — the *runtime* side of the deviation ratchet.
//!
//! `docs/security/ocap-deviations.md` defines the rule:
//!
//! > effective authority = meet( the human's grant , what the currently-verified
//! > invariants can actually enforce ).
//!
//! A dangerous capability is available **iff** all its required OCAP invariants
//! *verify*; otherwise it is **fail-closed OFF**, with honest evidence. A
//! *deviation* is an invariant currently **absent** (unbuilt). This module is the
//! runtime checker plus the fail-closed capability gates the register names
//! (`verify_b1`, `seed_live_credential`, …). CI's `just ocap-check`
//! (`scripts/ocap_check.py`) statically asserts that every `OCAP-DANGER:<id>`
//! site carries its `OCAP-GATE:<id>` while the deviation is open — so these gates
//! cannot be removed without turning the build red.
//!
//! Everything here is **fail-closed**: the verifiers return [`Verification::Absent`]
//! until the real OS-isolation / disclosure-filter / broker code lands, so the
//! dangerous paths are structurally unreachable — bounded *by construction*, not by
//! discipline. See `docs/design/ocap-enforcement.md` for the architecture.

use std::fmt;

/// The result of checking one OCAP invariant at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The invariant is enforced; `evidence` records how it was confirmed.
    Verified { evidence: String },
    /// The invariant is not yet enforced (an open deviation). Dependent
    /// capabilities stay fail-closed; `reason` is the honest "why".
    Absent {
        deviation: &'static str,
        reason: String,
    },
}

impl Verification {
    /// True only when the invariant is actually enforced.
    #[must_use]
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    /// The deviation id when absent (for honest banners / the ledger).
    #[must_use]
    pub fn deviation(&self) -> Option<&'static str> {
        match self {
            Self::Absent { deviation, .. } => Some(deviation),
            Self::Verified { .. } => None,
        }
    }
}

/// Refusal of a dangerous capability because a required invariant is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailClosed {
    pub deviation: &'static str,
    pub reason: String,
}

impl fmt::Display for FailClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refused (fail-closed): OCAP invariant '{}' is not enforced — {}",
            self.deviation, self.reason
        )
    }
}

impl std::error::Error for FailClosed {}

/// Require an invariant before proceeding; the fail-closed gate primitive.
fn require(v: Verification) -> Result<(), FailClosed> {
    match v {
        Verification::Verified { .. } => Ok(()),
        Verification::Absent { deviation, reason } => Err(FailClosed { deviation, reason }),
    }
}

/// The **b1-os-isolation** floor expressed as backend-neutral security
/// PROPERTIES — the invariants that must hold for an attacker-controlled child,
/// NOT the mechanisms that happen to establish them. A property may be proven on
/// Linux by Landlock / seccomp / cgroup v2 / a mediated broker, on macOS by
/// Seatbelt + a broker + a process-lifetime mechanism, on Windows by
/// AppContainer/LPAC + a Job Object + a broker. Two rules follow, and they are
/// the whole reason this is a property set rather than a mechanism list:
///
/// 1. A missing *optional* mechanism is **not** a failure if another mechanism
///    proves the same property. Unprivileged user/net namespaces are disabled by
///    host policy on hardened Linux (`apparmor_restrict_unprivileged_userns`,
///    Ubuntu ≥ 23.10), so [`DirectNetworkEgressDenied`] is proven there by a
///    seccomp `socket()`-family deny instead — a *complete* proof, not a
///    degraded one. uid-ns/netns are mechanisms, never theorem clauses.
/// 2. A mechanism merely being *available* is **not** proof the property holds
///    on the live path. Proof is per-session runtime evidence that the actual
///    attacker-exec route (`run_command` → agent-bridle shell) inherited the
///    enforcement — never a host-capability probe.
///
/// [`DirectNetworkEgressDenied`]: B1Property::DirectNetworkEgressDenied
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B1Property {
    /// Object-bounded filesystem authority — no read/write outside the fence.
    FilesystemConfinement,
    /// No DIRECT network egress: the child cannot itself open an off-box socket
    /// (TCP, UDP, DNS, raw, or packet).
    DirectNetworkEgressDenied,
    /// Any reachability is only through an explicit broker capability (a
    /// pre-opened handle), never ambient socket-creation authority.
    MediatedEgressOnly,
    /// The child starts from an empty environment plus only explicit grants.
    EnvironmentIsolation,
    /// No unintended inherited descriptor/handle (fd ≥ 3) crosses into the child.
    InheritedHandleIsolation,
    /// A hostile descendant cannot survive cancellation/timeout — the whole
    /// process subtree is owned and reaped.
    ProcessTreeContainment,
    /// No ambient credential (token, agent socket, key material) is reachable.
    CredentialIsolation,
    /// Tool-derived text is value-filtered before it can reach the model.
    DisclosureFiltering,
    /// Execution is REFUSED when a required property cannot be established — the
    /// floor never silently degrades.
    FailClosedEnforcement,
}

impl B1Property {
    /// Every property the b1 floor requires, in credential-safety order (the
    /// legs that keep a seeded token from leaving the box come first).
    pub const REQUIRED: [Self; 9] = [
        Self::FilesystemConfinement,
        Self::DirectNetworkEgressDenied,
        Self::MediatedEgressOnly,
        Self::CredentialIsolation,
        Self::EnvironmentIsolation,
        Self::InheritedHandleIsolation,
        Self::ProcessTreeContainment,
        Self::DisclosureFiltering,
        Self::FailClosedEnforcement,
    ];

    /// A stable slug for the property (used in verifier reasons + the register).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FilesystemConfinement => "filesystem-confinement",
            Self::DirectNetworkEgressDenied => "direct-network-egress-denied",
            Self::MediatedEgressOnly => "mediated-egress-only",
            Self::EnvironmentIsolation => "environment-isolation",
            Self::InheritedHandleIsolation => "inherited-handle-isolation",
            Self::ProcessTreeContainment => "process-tree-containment",
            Self::CredentialIsolation => "credential-isolation",
            Self::DisclosureFiltering => "disclosure-filtering",
            Self::FailClosedEnforcement => "fail-closed-enforcement",
        }
    }
}

/// The b1 floor as a per-route report: which [`B1Property`] invariants are
/// *proven* on a given execution path and which are not yet. This is the object
/// `verify_b1` reasons over — it replaces the old mechanism list
/// (`fs_fence`/`seccomp`/`uidns`/`netns`/`proxy`) so a backend that proves a
/// property by a *different* mechanism is never miscounted as a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct B1Floor {
    /// Properties NOT yet proven on this route, in [`B1Property::REQUIRED`]
    /// order. Empty iff the full semantic floor is proven on the live path.
    unmet: Vec<B1Property>,
}

impl B1Floor {
    /// The status of the **live attacker-exec path** (`run_command` →
    /// `dispatch_bridled_shell` → agent-bridle shell) today.
    ///
    /// That path spawns through agent-bridle's `ShellTool`, which applies a
    /// Landlock filesystem fence + the seccomp `socket()`-family egress deny
    /// (agent-bridle 0.7.15 `ChildNetworkPolicy::DenyDirect`, wired in slice 2).
    /// So of the required properties, as of the b1 slice stack:
    ///
    /// * [`DirectNetworkEgressDenied`] IS now proven on this path — no
    ///   AF_INET/AF_INET6/AF_PACKET socket can be created (seccomp), grounded by
    ///   `run_command_child_under_net_none_cannot_open_a_socket_b1`. (Not in the
    ///   unmet set below.)
    /// * [`MediatedEgressOnly`] is **not** proven — there is no mediated egress
    ///   broker (deferred, #1599), and worse, a confined child can still reach a
    ///   host AF_UNIX deputy (pathname + abstract; `af_unix_deputy.rs`), so egress
    ///   is not broker-only. This is the property `local-deputy-egress` tracks.
    /// * [`InheritedHandleIsolation`] is **not** proven on this path — the
    ///   run_command route's fd hygiene is CLOEXEC-based, not the explicit
    ///   `close_range(3,~0)` the `ConstrainedExecutor` `DenyAll` lane performs
    ///   (`run_command_route_fd_hygiene_is_cloexec_based_not_explicit_close`).
    /// * [`ProcessTreeContainment`] is **not** proven on this path — cgroup
    ///   subtree-kill is wired into the `ConstrainedExecutor` lane, not this shell
    ///   path.
    ///
    /// So `verify_b1` stays `Absent` naming [`MediatedEgressOnly`] — the
    /// credential-bearing floor genuinely still absent — never the (now-met)
    /// direct-egress property. This constructor reports the *known structural*
    /// gaps; it never manufactures a proof.
    ///
    /// [`DirectNetworkEgressDenied`]: B1Property::DirectNetworkEgressDenied
    /// [`MediatedEgressOnly`]: B1Property::MediatedEgressOnly
    /// [`InheritedHandleIsolation`]: B1Property::InheritedHandleIsolation
    /// [`ProcessTreeContainment`]: B1Property::ProcessTreeContainment
    #[must_use]
    pub fn live_attacker_path() -> Self {
        Self {
            unmet: vec![
                B1Property::MediatedEgressOnly,
                B1Property::InheritedHandleIsolation,
                B1Property::ProcessTreeContainment,
            ],
        }
    }

    /// The first unmet property, in credential-safety order. `None` iff the full
    /// semantic floor is proven on this route.
    #[must_use]
    pub fn first_unmet(&self) -> Option<B1Property> {
        B1Property::REQUIRED
            .into_iter()
            .find(|p| self.unmet.contains(p))
    }

    /// True iff every required property is proven on this route.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unmet.is_empty()
    }
}

/// Verify **b1-os-isolation** as a set of proven security PROPERTIES on the live
/// attacker-exec path — filesystem confinement, direct-egress denial,
/// mediated-egress-only, environment + credential isolation, inherited-handle
/// isolation, process-tree containment, disclosure filtering, and fail-closed
/// enforcement — each established by whatever mechanism the platform backend
/// uses (NOT a fixed uid-ns/netns stack). See [`B1Property`].
///
/// **Stays [`Verification::Absent`] by construction** for two independent
/// reasons, either of which alone forbids the flip:
///
/// 1. The live path (`run_command` → agent-bridle shell) does not yet prove the
///    network-egress properties — see [`B1Floor::live_attacker_path`].
/// 2. Proof is per-session *runtime evidence* that the actual child inherited
///    the enforcement; there is no such evidence seam yet, so no property can be
///    marked proven from host availability alone.
///
/// It is the SOLE remaining runtime lock on [`seed_live_credential`] +
/// [`admit_untrusted_remote`]; flipping it hollowly would admit a live token
/// into a still-reachable box. The flip is a later slice: the live `run_command`
/// path fenced through the agent-bridle child-network policy + a per-session
/// canary grounding test.
#[must_use]
pub fn verify_b1() -> Verification {
    let floor = B1Floor::live_attacker_path();
    let reason = match floor.first_unmet() {
        Some(p) => format!(
            "b1 credential-floor property not proven on the live attacker-exec path \
             (run_command → agent-bridle shell): {} — DIRECT off-box socket creation \
             IS now denied there (seccomp, agent-bridle 0.7.15 DenyDirect), but there \
             is no mediated-egress broker (deferred, #1599) and a confined child can \
             still reach a host AF_UNIX deputy (local-deputy-egress), so egress is not \
             broker-only; fd hygiene on this path is CLOEXEC-based and cgroup \
             subtree-kill is not wired here. b1 is a set of PROVEN properties, each met \
             by ANY sufficient mechanism, not a fixed uid-ns/netns stack",
            p.name()
        ),
        // Unreachable today (the live path has unmet properties). Even a complete
        // structural floor stays Absent until a per-session run records evidence.
        None => "b1 structural floor complete but live-session enforcement is \
                 unproven (per-session evidence seam + canary grounding test pending)"
            .to_string(),
    };
    Verification::Absent {
        deviation: "b1-os-isolation",
        reason,
    }
}

/// Verify **disclosure-gate-live-path**: every tool result passes a single
/// disclosure filter before it is pushed into `messages` (one chokepoint).
///
/// Whether the model-ingress disclosure backstop is in effect **on this
/// thread, right now** — probed, not asserted.
///
/// The mechanism is wired: step-6.1a put the by-value [`DisclosureFilter`]
/// in the tool-result chokepoint (`maybe_offload_tool_result`), and step-6.6
/// extended it to the summary path (`redact_model_facing`) and the memory /
/// observation / compaction / spill path (`redact_secrets`), the last of
/// which reaches its filter ONLY through the [`scoped_session_disclosure`]
/// thread-local.
///
/// That thread-local is why this must be a live probe. [`redact_session_ingress`]
/// is *the identity function* when no filter is installed on the calling
/// thread, and [`ScopedSessionDisclosure`] is `!Send` and thread-bound — so
/// "the backstop protects this text" is a property of a thread, not of the
/// build. A turn that ran on a thread which never installed the guard would
/// silently lose value-filtering on those paths.
///
/// Returns `Absent` — fail-closed, per this module's contract — whenever the
/// backstop cannot be shown to be working here. Outside a live turn (`newt
/// doctor`, startup, headless tooling) that is the *expected* answer and the
/// `reason` says so; it reports the runtime state, not a build defect.
///
/// **This function used to return `Verified` unconditionally** while citing
/// the TLS backstop as its evidence, which made it a vacuous check: it would
/// have reported exactly the same thing if the backstop had never been
/// installed anywhere. See `the_gate_refuses_to_claim_a_backstop_that_is_not_installed`.
#[must_use]
pub fn verify_disclosure_gate() -> Verification {
    const DEVIATION: &str = "disclosure-gate-live-path";
    SESSION_DISCLOSURE.with(|slot| match &*slot.borrow() {
        None => Verification::Absent {
            deviation: DEVIATION,
            reason: "no session disclosure filter is installed on this thread, so \
                     `redact_session_ingress` is the identity function here — the \
                     memory / observation / compaction / spill funnels would not \
                     value-filter. Expected outside a live turn; inside one it means \
                     the turn is running on a thread that never installed the guard."
                .into(),
        },
        Some(filter) if !filter.redacts_what_it_registered() => Verification::Absent {
            deviation: DEVIATION,
            reason: if filter.is_empty() {
                "a session disclosure filter is installed on this thread but has no \
                 registered secrets, so it would catch nothing"
                    .into()
            } else {
                "the session disclosure filter installed on this thread failed its own \
                 redaction probe — a registered value survived `redact`"
                    .into()
            },
        },
        Some(_) => Verification::Verified {
            evidence: "probed live on this thread: the installed session filter redacted \
                       every value it has registered, so the tool-result, summary, memory \
                       and repeat-steer model-ingress funnels all value-filter (the last \
                       via the TLS backstop); guarded by \
                       no_model_ingress_funnel_leaks_a_registered_session_secret + \
                       redact_secrets_value_filters_a_registered_session_secret + \
                       repeat_steer_value_filters_a_registered_session_secret"
                .into(),
        },
    })
}

/// A live, scoped credential to seed (the `pa login` use case): a short-lived
/// token a broker would present to outbound requests. The token VALUE is
/// deliberately not modelled here — the design keeps it *out of the box* (the
/// worker/model never sees it); only a non-secret `label` is carried for the
/// ledger.
#[derive(Debug, Clone)]
pub struct ScopedCredential {
    pub label: String,
}

/// Seed a live scoped credential into the agent's environment (`pa login`).
///
/// DANGEROUS: a live token with no OS sandbox is a direct token→internet
/// exfiltration path the instant the in-process monitor is bypassed, and the
/// token could surface to the model on the un-gated disclosure path. Per the
/// register it is **disabled while `b1-os-isolation` / `disclosure-gate-live-path`
/// are open**. Fail-closed: refuses unless both verify.
pub fn seed_live_credential(cred: &ScopedCredential) -> Result<(), FailClosed> {
    // OCAP-DANGER: b1-os-isolation — a live token with no OS sandbox is exfil-ready.
    // OCAP-GATE: b1-os-isolation
    require(verify_b1())?;
    // OCAP-DANGER: disclosure-gate-live-path — the token could reach the model raw.
    // OCAP-GATE: disclosure-gate-live-path
    require(verify_disclosure_gate())?;
    // (unreachable today) Both invariants verified: a broker now holds `cred` out
    // of the box and presents it to outbound requests; the value never enters the
    // model-facing environment.
    let _ = cred;
    Ok(())
}

/// Admit a genuinely-untrusted / foreign remote voice that may hold anything
/// sensitive (a future remote swarm peer).
///
/// DANGEROUS without the OS sandbox: a hostile voice with no containment can
/// escalate. **Disabled while `b1-os-isolation` is open.** Fail-closed.
pub fn admit_untrusted_remote(voice_fingerprint: &str) -> Result<(), FailClosed> {
    // OCAP-DANGER: b1-os-isolation — an untrusted voice needs OS containment.
    // OCAP-GATE: b1-os-isolation
    require(verify_b1())?;
    let _ = voice_fingerprint;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b1_verifier_absent_until_built() {
        // b1 remains fail-closed until the kernel floor lands (P5).
        assert!(!verify_b1().is_verified());
        assert_eq!(verify_b1().deviation(), Some("b1-os-isolation"));
    }

    #[test]
    fn b1floor_live_path_reports_network_egress_properties_unmet() {
        let floor = B1Floor::live_attacker_path();
        // The live run_command -> bridle-shell path now PROVES direct-egress denial
        // (seccomp, slice 2), so it is no longer in the unmet set; the first
        // genuinely-absent property is mediated-egress-only (the deferred broker).
        assert!(
            !floor.is_complete(),
            "the live path is not fully fenced yet"
        );
        assert!(
            !floor.unmet.contains(&B1Property::DirectNetworkEgressDenied),
            "direct-egress denial is proven on the live path now — it must not be unmet"
        );
        assert_eq!(
            floor.first_unmet(),
            Some(B1Property::MediatedEgressOnly),
            "the credential-floor broker (mediated-egress-only) is the first remaining gap"
        );
    }

    #[test]
    fn verify_b1_names_an_unmet_property_not_a_mechanism() {
        // Honest-as-code: the reason names a semantic PROPERTY that is unproven on
        // the live path — never the old blanket "no OS sandbox", and it states
        // the floor is property-based (uid-ns/netns are not mandatory clauses).
        let v = verify_b1();
        assert!(!v.is_verified());
        assert_eq!(v.deviation(), Some("b1-os-isolation"));
        let Verification::Absent { reason, .. } = &v else {
            unreachable!("b1 is Absent")
        };
        assert!(
            reason.contains(B1Property::MediatedEgressOnly.name()),
            "reason should name the unmet credential-floor property (mediated-egress-only): {reason}"
        );
        assert!(
            !reason.contains("seccomp egress floor / mediated broker / fd hygiene"),
            "reason must not carry the stale 'seccomp floor not established' wording — direct \
             egress IS proven now: {reason}"
        );
        assert!(
            reason.contains("not a fixed uid-ns/netns stack"),
            "reason must state b1 is property-based, not mechanism-locked: {reason}"
        );
        assert!(
            !reason.contains("no OS sandbox"),
            "reason must be property-specific, not the old blanket constant: {reason}"
        );
    }

    #[test]
    fn b1property_required_is_credential_safety_ordered_and_first_unmet_follows_it() {
        // Filesystem confinement is the base; the egress/broker/credential legs
        // come before the lifecycle + disclosure + fail-closed legs.
        assert_eq!(B1Property::REQUIRED[0], B1Property::FilesystemConfinement);
        assert_eq!(
            B1Property::REQUIRED[1],
            B1Property::DirectNetworkEgressDenied
        );
        assert_eq!(B1Property::REQUIRED[2], B1Property::MediatedEgressOnly);
        assert_eq!(B1Property::REQUIRED[3], B1Property::CredentialIsolation);

        // A complete floor has no gap.
        let complete = B1Floor { unmet: vec![] };
        assert!(complete.is_complete());
        assert_eq!(complete.first_unmet(), None);

        // first_unmet follows REQUIRED order regardless of the Vec's order:
        // process-tree containment is last, so with only it missing it is named.
        let only_lifecycle = B1Floor {
            unmet: vec![B1Property::ProcessTreeContainment],
        };
        assert_eq!(
            only_lifecycle.first_unmet(),
            Some(B1Property::ProcessTreeContainment)
        );
        // With both egress denial and containment missing (Vec in reverse order),
        // the earlier-ordered egress-denial property is named first.
        let mixed = B1Floor {
            unmet: vec![
                B1Property::ProcessTreeContainment,
                B1Property::DirectNetworkEgressDenied,
            ],
        };
        assert_eq!(
            mixed.first_unmet(),
            Some(B1Property::DirectNetworkEgressDenied)
        );
    }

    /// A filter for probe tests. High-entropy, obviously synthetic.
    fn probe_filter() -> DisclosureFilter {
        let mut f = DisclosureFilter::new();
        f.register("sk-probe-9f3a2b7c1d4e6a8b");
        f
    }

    /// The gate answers the question it is asked — "is the backstop working
    /// HERE" — rather than restating a build-time belief.
    ///
    /// This one test walks all three states in sequence because the contrast
    /// IS the assertion: the pre-fix implementation returned `Verified` in all
    /// three, so any test that exercised only one of them would have passed
    /// against a function that never looked at anything.
    #[test]
    fn the_gate_distinguishes_the_three_backstop_states() {
        // 1. Nothing installed → the identity function → refuse to claim.
        assert!(
            !verify_disclosure_gate().is_verified(),
            "with no filter on this thread, redact_session_ingress is identity"
        );
        assert_eq!(
            verify_disclosure_gate().deviation(),
            Some("disclosure-gate-live-path")
        );

        // 2. Installed but registering nothing → catches nothing → refuse.
        {
            let _g = scoped_session_disclosure(DisclosureFilter::new());
            assert!(!verify_disclosure_gate().is_verified());
        }

        // 3. Installed and provably redacting → verified, no deviation.
        {
            let _g = scoped_session_disclosure(probe_filter());
            let v = verify_disclosure_gate();
            assert!(v.is_verified(), "{v:?}");
            assert_eq!(v.deviation(), None);
        }

        // And the guard restores: back to state 1.
        assert!(!verify_disclosure_gate().is_verified());
    }

    /// The regression this fix exists for.
    ///
    /// `verify_disclosure_gate` used to return `Verified` unconditionally,
    /// with evidence text that explicitly cited the `scoped_session_disclosure`
    /// TLS backstop — while performing no check at all. It would have reported
    /// exactly the same thing if the backstop had never been installed
    /// anywhere, which is the definition of a vacuous check.
    #[test]
    fn the_gate_refuses_to_claim_a_backstop_that_is_not_installed() {
        let v = verify_disclosure_gate();
        assert!(!v.is_verified());
        let reason = match &v {
            Verification::Absent { reason, .. } => reason.clone(),
            Verification::Verified { .. } => unreachable!("just asserted not verified"),
        };
        assert!(
            reason.contains("identity function"),
            "the reason must name the actual consequence, got: {reason}"
        );
    }

    /// The property that protects the concurrency work (#1669 / the cockpit
    /// train): `ScopedSessionDisclosure` is `!Send` and thread-bound, so a
    /// turn that migrates onto a thread which never installed the guard loses
    /// value-filtering on the memory / observation / compaction / spill path.
    /// The gate must report that, not inherit the installing thread's claim.
    #[test]
    fn the_backstop_does_not_follow_the_gate_onto_another_thread() {
        let _g = scoped_session_disclosure(probe_filter());
        assert!(
            verify_disclosure_gate().is_verified(),
            "installed on this thread"
        );

        let elsewhere = std::thread::spawn(|| verify_disclosure_gate().is_verified())
            .join()
            .expect("probe thread");
        assert!(
            !elsewhere,
            "the TLS backstop is per-thread — the gate must never claim it on a \
             thread that did not install it"
        );
    }

    /// The probe runs the real machinery, so it tracks `redact`'s documented
    /// post-condition across every encoding rather than spot-checking the raw
    /// form. A filter that only caught the raw value would not verify.
    #[test]
    fn the_probe_covers_every_tracked_encoding() {
        let f = probe_filter();
        assert!(f.redacts_what_it_registered());
        // Non-vacuous control: the same predicate is false when there is
        // nothing registered, so it is reading state rather than returning a
        // constant.
        assert!(!DisclosureFilter::new().redacts_what_it_registered());
        assert!(DisclosureFilter::new().is_empty());
        assert!(!f.is_empty());
    }

    #[test]
    fn verified_reports_no_deviation() {
        let v = Verification::Verified {
            evidence: "synthetic".into(),
        };
        assert!(v.is_verified());
        assert_eq!(v.deviation(), None);
    }

    #[test]
    fn network_confinement_is_the_basic_floor_not_the_credential_floor() {
        // The split: the basic network floor is Verified where the seccomp +
        // Landlock egress floor is enforceable, while the credential-bearing b1
        // floor stays Absent (so credential-seeding gates keep failing closed).
        let net = verify_network_confinement();
        if crate::confined_exec::kernel_fs_fence_available() {
            assert!(
                net.is_verified(),
                "basic network confinement should be enforced here"
            );
            assert_eq!(net.deviation(), None);
        } else {
            assert!(!net.is_verified());
            assert_eq!(net.deviation(), Some("b1-os-isolation"));
        }
        // The stronger credential-bearing floor is independent and still open.
        assert!(!verify_b1().is_verified());
        assert_eq!(verify_b1().deviation(), Some("b1-os-isolation"));
    }

    #[test]
    fn seed_live_credential_fails_closed_on_b1() {
        let cred = ScopedCredential {
            label: "pa-token".into(),
        };
        let err = seed_live_credential(&cred).unwrap_err();
        assert_eq!(err.deviation, "b1-os-isolation");
        assert!(err.to_string().contains("fail-closed"));
    }

    #[test]
    fn admit_untrusted_remote_fails_closed() {
        let err = admit_untrusted_remote("SHA256:deadbeef").unwrap_err();
        assert_eq!(err.deviation, "b1-os-isolation");
    }

    #[test]
    fn require_passes_only_when_verified() {
        assert!(require(Verification::Verified {
            evidence: "ok".into()
        })
        .is_ok());
        assert!(require(verify_b1()).is_err());
    }
}

// ===========================================================================
// Disclosure filter — the by-VALUE redaction primitive for
// `disclosure-gate-live-path` (docs/design/ocap-enforcement.md §3).
//
// Threat-model finding: filter by KNOWN VALUE, not by shape. A shape filter
// ("looks like a token") both over-blocks and is **defeated by re-encoding**; a
// value filter catches the registered secret's actual bytes in any common
// encoding. This is the mechanism `verify_disclosure_gate` will assert on the
// live tool-result path — the canary: a value seeded at session start must never
// reach a model-facing message, in ANY encoding. (Wiring it into the live
// `messages` chokepoint lives in the agentic loop — a follow-up, kept out of
// this module to avoid colliding with concurrent work there.)
// ===========================================================================

use base64::Engine as _;

/// Redacts known secret VALUES — and their common re-encodings — from text
/// before it reaches the model. Register the live token / session canary;
/// [`leaks`](Self::leaks) detects and [`redact`](Self::redact) removes it.
#[derive(Debug, Default, Clone)]
pub struct DisclosureFilter {
    secrets: Vec<String>,
}

impl DisclosureFilter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a secret value to catch (raw + re-encoded). Empty values are
    /// ignored. Secrets should be high-entropy (tokens/canaries) — a value
    /// filter trusts the caller not to register common substrings.
    pub fn register(&mut self, secret: impl Into<String>) {
        let s = secret.into();
        if !s.is_empty() {
            self.secrets.push(s);
        }
    }

    /// The concrete forms of `secret` we match: the raw value plus its common
    /// re-encodings — base64 (standard + url-safe, padded + unpadded), hex
    /// (lower + upper), percent-encoding (upper + lower), and the `\xXX` /
    /// `\uXXXX` string escapes. A model bent on exfiltration re-encodes; a
    /// VALUE filter follows the value through each transform rather than
    /// guessing at a shape. Deduped — short secrets collide across some forms.
    ///
    /// The `\uXXXX` form escapes per byte (`\u00XX`), which matches a real JSON
    /// escape only for ASCII secrets; non-ASCII tokens are still caught by the
    /// raw / base64 / hex forms.
    fn encodings(secret: &str) -> Vec<String> {
        use base64::engine::general_purpose::{
            STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
        };
        let bytes = secret.as_bytes();
        let hex_lower: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let hex_upper: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
        let pct_upper: String = bytes.iter().map(|b| format!("%{b:02X}")).collect();
        let pct_lower: String = bytes.iter().map(|b| format!("%{b:02x}")).collect();
        let esc_x: String = bytes.iter().map(|b| format!("\\x{b:02x}")).collect();
        let esc_u: String = bytes.iter().map(|b| format!("\\u{b:04x}")).collect();
        let mut forms = vec![
            secret.to_string(),
            STANDARD.encode(bytes),
            STANDARD_NO_PAD.encode(bytes),
            URL_SAFE.encode(bytes),
            URL_SAFE_NO_PAD.encode(bytes),
            hex_lower,
            hex_upper,
            pct_upper,
            pct_lower,
            esc_x,
            esc_u,
        ];
        forms.sort();
        forms.dedup();
        forms
    }

    /// The text with all Unicode whitespace removed — the normalisation that
    /// defeats a **chunk-split** obfuscation, where a secret (or one of its
    /// encodings) is broken across whitespace: `CANARY\n-7f3a…`, a line-wrapped
    /// base64 blob, hex split every N chars. Scanning both the raw text and its
    /// whitespace-stripped form catches the split without a reflow pass.
    fn strip_ws(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// Does `text` disclose any registered secret, in any tracked encoding —
    /// including a **chunk-split** occurrence broken across whitespace? This is
    /// the authoritative gate decision; the live path withholds a result for
    /// which this returns `true`.
    #[must_use]
    pub fn leaks(&self, text: &str) -> bool {
        let normalized = Self::strip_ws(text);
        self.secrets.iter().any(|s| {
            Self::encodings(s)
                .iter()
                .any(|e| text.contains(e.as_str()) || normalized.contains(e.as_str()))
        })
    }

    /// Replace every occurrence of every registered secret (raw or re-encoded)
    /// with `[REDACTED]`. Contiguous forms are excised inline; a **chunk-split**
    /// occurrence can't be excised without reflowing the text, so if any secret
    /// still surfaces once whitespace is normalised away, the whole text is
    /// withheld (fail closed). The post-condition `!self.leaks(&self.redact(t))`
    /// therefore holds for *every* input, contiguous or split.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            for enc in Self::encodings(s) {
                out = out.replace(enc.as_str(), "[REDACTED]");
            }
        }
        if self.leaks(&out) {
            return "[REDACTED: withheld — disclosed a registered secret]".to_string();
        }
        out
    }

    /// Has anything been registered? A filter with no secrets catches nothing,
    /// so it is not a backstop even though it is installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Prove — by running the real machinery — that this filter redacts every
    /// value it has registered, in every tracked encoding.
    ///
    /// This is what makes [`verify_disclosure_gate`] a check rather than a
    /// claim. It asserts [`redact`](Self::redact)'s documented post-condition
    /// (`!leaks(&redact(t))`) using the filter's own authoritative
    /// [`leaks`](Self::leaks) decision, so a regression in either direction —
    /// an encoding that stops being excised, or a `leaks` that stops seeing
    /// one — turns the gate `Absent` instead of leaving it green.
    ///
    /// No registered value escapes: the probe strings are built, redacted and
    /// judged entirely inside this method, and only a `bool` leaves it.
    /// `false` for an empty filter — there is nothing to prove.
    #[must_use]
    pub fn redacts_what_it_registered(&self) -> bool {
        !self.secrets.is_empty()
            && self.secrets.iter().all(|s| {
                Self::encodings(s).iter().all(|enc| {
                    // A realistic carrier, so the probe exercises inline
                    // excision rather than a whole-string match.
                    let probe = format!("probe prefix {enc} probe suffix");
                    !self.leaks(&self.redact(&probe))
                })
            })
    }
}

/// Build the session's model-ingress [`DisclosureFilter`], registering the live
/// secret VALUES the worker actually holds so they are redacted before any tool
/// result or summary reaches the model. Today that is the provider API key /
/// bearer token (`api_key`); the signature is a seam for the other session
/// secrets (operator key, MCP credential handles) as they are threaded.
///
/// A short/empty/placeholder key is NOT registered — a real bearer token is
/// long and high-entropy, and registering a trivial value would over-redact
/// benign text. The returned filter is inert (redacts nothing) when there is no
/// secret to guard, so wiring it into `ChatCtx.disclosure` is bit-for-bit safe.
#[must_use]
pub fn session_disclosure_filter(api_key: Option<&str>) -> DisclosureFilter {
    let mut filter = DisclosureFilter::new();
    if let Some(key) = api_key {
        if key.len() >= 8 {
            filter.register(key);
        }
    }
    filter
}

std::thread_local! {
    /// The current thread's session disclosure filter. A driven turn installs it
    /// before crossing onto its dedicated thread (same pattern as the effective-
    /// tenacity override), so every model-ingress redaction path on that thread —
    /// the tool-result chokepoint, summaries, AND the memory/observation/
    /// compaction/spill path that funnels through `redact_secrets` — value-filters
    /// against the same registered secrets, regardless of which path assembled the
    /// text. This is the "no alternate path" backstop for the sinks that do not
    /// carry an explicit `&DisclosureFilter`.
    static SESSION_DISCLOSURE: std::cell::RefCell<Option<DisclosureFilter>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the prior current-thread session filter on drop. The `Rc` marker
/// keeps the guard on the thread whose TLS slot it owns; driven turns use a
/// current-thread runtime, so the guard safely spans the whole async turn.
#[must_use]
pub struct ScopedSessionDisclosure {
    previous: Option<DisclosureFilter>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for ScopedSessionDisclosure {
    fn drop(&mut self) {
        let prev = self.previous.take();
        let _ = SESSION_DISCLOSURE.try_with(|slot| *slot.borrow_mut() = prev);
    }
}

/// Install `filter` as the current thread's session disclosure filter until the
/// returned guard drops. Every by-value model-ingress redaction on this thread
/// then consults it. Nests in lexical (LIFO) order.
pub fn scoped_session_disclosure(filter: DisclosureFilter) -> ScopedSessionDisclosure {
    let previous = SESSION_DISCLOSURE.with(|slot| slot.borrow_mut().replace(filter));
    ScopedSessionDisclosure {
        previous,
        _thread_bound: std::marker::PhantomData,
    }
}

/// Redact `text` through the current thread's session disclosure filter, if one
/// is installed. The by-value gate for model-ingress paths that don't carry an
/// explicit `&DisclosureFilter` (memory / observation / compaction / spill).
/// Identity when no session filter is installed.
#[must_use]
pub fn redact_session_ingress(text: &str) -> String {
    SESSION_DISCLOSURE.with(|slot| match &*slot.borrow() {
        Some(filter) => filter.redact(text),
        None => text.to_string(),
    })
}

#[cfg(test)]
mod disclosure_tests {
    use super::*;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }
    fn hexs(s: &str) -> String {
        s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn session_filter_registers_a_real_provider_key() {
        // The live session filter catches the provider bearer token — raw and
        // re-encoded — so a tool result or summary echoing it is redacted.
        let f = session_disclosure_filter(Some("sk-live-9f3a2b7c1d"));
        assert!(f.leaks("Authorization: Bearer sk-live-9f3a2b7c1d"));
        assert!(f.leaks(&format!("k={}", b64("sk-live-9f3a2b7c1d"))));
        assert!(f.redact("token=sk-live-9f3a2b7c1d").contains("[REDACTED]"));
    }

    #[test]
    fn session_filter_ignores_trivial_or_absent_key() {
        // No key → inert (bit-for-bit safe to wire everywhere).
        assert!(!session_disclosure_filter(None).leaks("anything at all"));
        // A short/placeholder value is NOT registered — registering it would
        // over-redact benign text.
        assert!(!session_disclosure_filter(Some("x")).leaks("x marks the spot"));
    }

    #[test]
    fn session_tls_redacts_installed_secret_and_restores() {
        // No filter installed on this thread → identity.
        assert_eq!(
            redact_session_ingress("plain sk-live-abc12345"),
            "plain sk-live-abc12345"
        );
        {
            let mut f = DisclosureFilter::new();
            f.register("sk-live-abc12345");
            let _g = scoped_session_disclosure(f);
            // Installed → the registered secret (raw + re-encoded) is redacted on
            // ANY model-ingress path that consults the TLS.
            assert!(!redact_session_ingress("token=sk-live-abc12345").contains("sk-live-abc12345"));
            let enc = b64("sk-live-abc12345");
            assert!(!redact_session_ingress(&format!("b={enc}")).contains(&enc));
        }
        // Guard dropped → restored to identity (no leak of the guard across turns).
        assert_eq!(
            redact_session_ingress("token=sk-live-abc12345"),
            "token=sk-live-abc12345"
        );
    }

    #[test]
    fn catches_raw_value() {
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        assert!(f.leaks("the token is CANARY-7f3a9c2b right here"));
        assert!(!f.leaks("nothing secret in this text"));
    }

    #[test]
    fn catches_base64_reencoding() {
        // The key property: re-encoding defeats a SHAPE filter, not a VALUE filter.
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        let leaked = format!("here is {} encoded", b64("CANARY-7f3a9c2b"));
        assert!(f.leaks(&leaked), "base64 re-encoding must still be caught");
    }

    #[test]
    fn catches_hex_reencoding() {
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        assert!(f.leaks(&format!("payload={}", hexs("CANARY-7f3a9c2b"))));
    }

    #[test]
    fn redacts_all_forms() {
        let mut f = DisclosureFilter::new();
        f.register("SECRETVAL-abc123");
        let text = format!(
            "raw=SECRETVAL-abc123 b64={} hex={}",
            b64("SECRETVAL-abc123"),
            hexs("SECRETVAL-abc123")
        );
        let red = f.redact(&text);
        assert!(!f.leaks(&red), "redacted text must not leak");
        assert!(red.contains("[REDACTED]"));
    }

    #[test]
    fn value_filter_not_shape_filter() {
        // An UNREGISTERED token-shaped string is NOT flagged — we filter by known
        // value, not "looks like a secret". (The deliberate threat-model choice.)
        let f = DisclosureFilter::new();
        assert!(!f.leaks("AKIAIOSFODNN7EXAMPLE looks like a key but isn't registered"));
    }

    #[test]
    fn empty_registration_is_ignored() {
        let mut f = DisclosureFilter::new();
        f.register("");
        assert!(!f.leaks("anything at all"));
    }

    // ── Full re-encoding matrix (mandate: raw, base64/base64url, hex, escaped,
    //    URL-encoded, chunk-split) ─────────────────────────────────────────────

    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

    /// A secret whose bytes force `+`/`/` in standard base64, so its base64url
    /// form genuinely differs (the `-`/`_` alphabet). The non-ASCII tail
    /// guarantees it.
    const B64_DISTINCT: &str = "canary-\u{00ff}\u{00fe}\u{00fd}";

    #[test]
    fn catches_base64url_reencoding() {
        let mut f = DisclosureFilter::new();
        f.register(B64_DISTINCT);
        let url = URL_SAFE.encode(B64_DISTINCT.as_bytes());
        let std = base64::engine::general_purpose::STANDARD.encode(B64_DISTINCT.as_bytes());
        assert_ne!(
            url, std,
            "test secret must distinguish base64 from base64url"
        );
        assert!(
            f.leaks(&format!("payload={url}")),
            "base64url must be caught"
        );
        assert!(f.leaks(&format!(
            "payload={}",
            URL_SAFE_NO_PAD.encode(B64_DISTINCT.as_bytes())
        )));
    }

    #[test]
    fn catches_base64_nopad_reencoding() {
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        assert!(f.leaks(&format!(
            "b={}",
            STANDARD_NO_PAD.encode("CANARY-7f3a9c2b".as_bytes())
        )));
    }

    #[test]
    fn catches_uppercase_hex() {
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        let upper: String = "CANARY-7f3a9c2b"
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        assert!(f.leaks(&format!("HX={upper}")));
    }

    #[test]
    fn catches_percent_encoding() {
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        let pct: String = "CANARY-7f3a9c2b"
            .as_bytes()
            .iter()
            .map(|b| format!("%{b:02X}"))
            .collect();
        assert!(
            f.leaks(&format!("q={pct}")),
            "URL/percent-encoding must be caught"
        );
    }

    #[test]
    fn catches_string_escapes() {
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        let esc_x: String = "CANARY-7f3a9c2b"
            .as_bytes()
            .iter()
            .map(|b| format!("\\x{b:02x}"))
            .collect();
        let esc_u: String = "CANARY-7f3a9c2b"
            .as_bytes()
            .iter()
            .map(|b| format!("\\u{b:04x}"))
            .collect();
        assert!(
            f.leaks(&format!("s=\"{esc_x}\"")),
            "\\xXX escape must be caught"
        );
        assert!(
            f.leaks(&format!("s=\"{esc_u}\"")),
            "\\uXXXX escape must be caught"
        );
    }

    #[test]
    fn catches_chunk_split_raw() {
        // The secret broken across whitespace (newline/space) — a shape a model
        // uses to slip a value past a naive contiguous scan.
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        assert!(f.leaks("prefix CANARY-\n7f3a9c2b suffix"));
        assert!(f.leaks("C A N A R Y - 7 f 3 a 9 c 2 b"));
    }

    #[test]
    fn catches_chunk_split_base64() {
        // A line-wrapped base64 blob (the classic MIME 76-col wrap) must still
        // be caught once whitespace is normalised.
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        let b64 = base64::engine::general_purpose::STANDARD.encode("CANARY-7f3a9c2b".as_bytes());
        let mid = b64.len() / 2;
        let wrapped = format!("{}\n{}", &b64[..mid], &b64[mid..]);
        assert!(f.leaks(&wrapped), "line-wrapped base64 must be caught");
    }

    #[test]
    fn redact_withholds_chunk_split() {
        // Chunk-split can't be excised inline; redact must fail closed by
        // withholding the whole text, so the post-condition holds for split too.
        let mut f = DisclosureFilter::new();
        f.register("CANARY-7f3a9c2b");
        let split = "leak: CANARY-\n7f3a9c2b done";
        let red = f.redact(split);
        assert!(!f.leaks(&red), "redacted split text must not leak");
        assert!(
            red.contains("withheld"),
            "split redaction withholds wholesale"
        );
    }

    #[test]
    fn redact_post_condition_holds_for_every_form() {
        // The invariant that makes redact safe to forward: its output never
        // leaks, across the whole encoding matrix + a chunk split.
        let mut f = DisclosureFilter::new();
        f.register("SECRETVAL-abc123");
        let s = "SECRETVAL-abc123";
        let b64 = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        let hex: String = s.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let pct: String = s.as_bytes().iter().map(|b| format!("%{b:02X}")).collect();
        let text = format!("raw={s} b64={b64} hex={hex} pct={pct} split=SEC\nRETVAL-abc123");
        assert!(!f.leaks(&f.redact(&text)), "redact output must never leak");
    }
}

// ===========================================================================
// Separation of duties — `sod-proposer-not-worker`
// (docs/security/ocap-deviations.md §sod, docs/design/ocap-enforcement.md §5).
//
// The policy-proposing surface must be a cryptographically DISTINCT, more-trusted
// identity than the confined worker — otherwise observe-then-propose lets the
// worker author its own ceiling (privilege escalation by self-proposal). The
// distinctness half is checkable now (`proposer_distinct`); the taint-aware
// observe-then-propose half is UNBUILT, so `verify_sod` stays Absent
// (fail-closed) and `auto_apply_policy` refuses regardless.
// ===========================================================================

/// The distinctness primitive: a non-empty proposer fingerprint different from
/// the worker's. **Necessary, not sufficient**, for separation of duties.
#[must_use]
pub fn proposer_distinct(proposer_fp: &str, worker_fp: &str) -> bool {
    !proposer_fp.is_empty() && proposer_fp != worker_fp
}

/// Verify **sod-proposer-not-worker**: a distinct, more-trusted proposer key
/// (`proposer_fp != worker_fp`) AND taint-aware observe-then-propose. The
/// distinctness half is checked here; the taint-aware half is unbuilt, so this
/// stays [`Verification::Absent`] — but the `reason` reports the distinctness
/// state so the ledger is honest. Flips to `Verified` when taint-awareness lands.
#[must_use]
pub fn verify_sod(proposer_fp: &str, worker_fp: &str) -> Verification {
    if !proposer_distinct(proposer_fp, worker_fp) {
        return Verification::Absent {
            deviation: "sod-proposer-not-worker",
            reason: "proposer key is not distinct from the worker (self-proposal)".into(),
        };
    }
    Verification::Absent {
        deviation: "sod-proposer-not-worker",
        reason: "distinct proposer key confirmed, but taint-aware observe-then-propose is unbuilt"
            .into(),
    }
}

/// Auto-apply a proposed policy (lower/raise a worker's `Caveats` without a human).
///
/// DANGEROUS: with no separation of duties this is privilege escalation by
/// self-proposal. **Disabled while `sod-proposer-not-worker` is open** — every
/// promotion needs a human approval bound to the lowered-`Caveats` hash. Fail-closed.
pub fn auto_apply_policy(proposer_fp: &str, worker_fp: &str) -> Result<(), FailClosed> {
    // OCAP-DANGER: sod-proposer-not-worker — auto-apply enables self-proposal escalation.
    // OCAP-GATE: sod-proposer-not-worker
    require(verify_sod(proposer_fp, worker_fp))?;
    Ok(())
}

// ===========================================================================
// Achieved-security capability report — the typed, per-guarantee posture model
// (P8; adversarial targets #11 "reporting matches enforcement" and #12
// "unsupported platforms cannot claim Linux-equivalent OCAP").
//
// The report answers, for the ACTIVE platform and session, seven independent
// questions — fs / process / network confinement, env / credential isolation,
// disclosure filtering, fail-closed execution — each with one of three honest
// answers: Enforced (with evidence), Unverified (open deviation, named), or
// Unsupported (the platform cannot provide it; permanently fail-closed here).
//
// Two laws, both fail-closed:
//
//   achieved = meet( platform ceiling , runtime verification )
//
// 1. **The ceiling never rounds up.** A `Verified` runtime verifier on a
//    platform whose ceiling says "cannot provide" still reports Unsupported —
//    an unsupported platform can never claim Linux-equivalent OCAP (#12).
// 2. **Reporting is derived, never asserted.** Every entry comes from the same
//    verifiers the capability gates use (`verify_b1`, `verify_disclosure_gate`,
//    …), so a posture surface rendering this report cannot drift from actual
//    enforcement (#11). There is no constructor that takes a free-form claim.
//
// Knowledge lives in data (three Cs): each platform's ceiling is a pure const
// table, and unknown platforms get the all-Unsupported ceiling — the default
// arm is the MOST restrictive, never `_ => true`.
// ===========================================================================

/// The seven independently-reported guarantees of the achieved-security report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Guarantee {
    /// Filesystem authority is object-bound beneath the workspace root.
    FsConfinement,
    /// Untrusted child processes run under kernel-backed confinement.
    ProcessConfinement,
    /// Untrusted code has no network egress without an explicit capability.
    NetworkConfinement,
    /// Untrusted children start from a cleared, minimal environment.
    EnvIsolation,
    /// Ambient credentials are unreachable from untrusted code.
    CredentialIsolation,
    /// Registered secret values are filtered from every model-ingress funnel.
    DisclosureFiltering,
    /// A missing required guarantee refuses the operation (no silent downgrade).
    FailClosedExecution,
}

impl Guarantee {
    /// Every guarantee, for exhaustive iteration (reports, banners, audits).
    pub const ALL: [Self; 7] = [
        Self::FsConfinement,
        Self::ProcessConfinement,
        Self::NetworkConfinement,
        Self::EnvIsolation,
        Self::CredentialIsolation,
        Self::DisclosureFiltering,
        Self::FailClosedExecution,
    ];

    /// Stable short label for banners and logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::FsConfinement => "fs-confinement",
            Self::ProcessConfinement => "process-confinement",
            Self::NetworkConfinement => "network-confinement",
            Self::EnvIsolation => "env-isolation",
            Self::CredentialIsolation => "credential-isolation",
            Self::DisclosureFiltering => "disclosure-filtering",
            Self::FailClosedExecution => "fail-closed-execution",
        }
    }
}

/// What the active platform + session actually achieve for one guarantee.
/// There is deliberately no "best effort" variant — a guarantee is enforced
/// with evidence, open with a named deviation, or unsupported. Nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Achieved {
    /// Actually enforced this session; `evidence` records how.
    Enforced { evidence: String },
    /// The platform could provide it, but the enforcement is unbuilt or
    /// unproven — the named open deviation in the register.
    Unverified {
        deviation: &'static str,
        reason: String,
    },
    /// The platform cannot provide it. Permanently fail-closed here; an
    /// operation requiring it must refuse (never silently degrade).
    Unsupported { reason: String },
}

impl Achieved {
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforced { .. })
    }
}

/// A platform's *capability ceiling*: for each guarantee, either "the platform
/// can support it — let the runtime verifier decide" (`None`) or "the platform
/// cannot provide it" with the honest reason (`Some`). Pure data, one const
/// per platform; the meet with runtime evidence never exceeds this ceiling.
#[derive(Debug)]
pub struct PlatformCeiling {
    pub platform: &'static str,
    /// `(guarantee, None | Some(cannot-provide reason))`, one row per guarantee.
    table: [(Guarantee, Option<&'static str>); 7],
}

impl PlatformCeiling {
    /// `Some(reason)` when the platform cannot provide `g`; `None` when the
    /// runtime verifier decides. A guarantee missing from the table is treated
    /// as unsupported — absence of knowledge is not a grant.
    #[must_use]
    pub fn cannot_provide(&self, g: Guarantee) -> Option<&'static str> {
        for (row, reason) in &self.table {
            if *row == g {
                return *reason;
            }
        }
        Some("guarantee not present in this platform's ceiling table")
    }
}

/// Linux: the normative fully-supported OCAP platform. Every guarantee is
/// supportable; the runtime verifiers decide what is actually achieved.
pub const LINUX_CEILING: PlatformCeiling = PlatformCeiling {
    platform: "linux",
    table: [
        (Guarantee::FsConfinement, None),
        (Guarantee::ProcessConfinement, None),
        (Guarantee::NetworkConfinement, None),
        (Guarantee::EnvIsolation, None),
        (Guarantee::CredentialIsolation, None),
        (Guarantee::DisclosureFiltering, None),
        (Guarantee::FailClosedExecution, None),
    ],
};

/// macOS: explicitly NOT Linux-equivalent for this milestone. Object-bound fs
/// (`openat2`) does not exist; the Seatbelt-backed kernel floor is unbuilt and
/// unverified (no runner), so kernel-backed guarantees are unsupported — honest
/// and fail-closed, per the "Linux is normative" ADR. Process-independent
/// guarantees (disclosure filtering, fail-closed gating) remain supportable.
pub const MACOS_CEILING: PlatformCeiling = PlatformCeiling {
    platform: "macos",
    table: [
        (
            Guarantee::FsConfinement,
            Some("no openat2(RESOLVE_BENEATH); lexical fallback is not object-bound"),
        ),
        (
            Guarantee::ProcessConfinement,
            Some("Seatbelt floor unbuilt/unverified (no macOS runner); B1 unsupported here"),
        ),
        (
            Guarantee::NetworkConfinement,
            Some("no kernel-backed default-deny egress on this platform"),
        ),
        (Guarantee::EnvIsolation, None),
        (
            Guarantee::CredentialIsolation,
            Some("without fs/process confinement, ambient credentials are reachable"),
        ),
        (Guarantee::DisclosureFiltering, None),
        (Guarantee::FailClosedExecution, None),
    ],
};

/// Windows: same posture as macOS — kernel-backed guarantees unsupported
/// (no openat2/Landlock; AppContainer unbuilt), process-independent ones
/// supportable. Never claims Linux-equivalent OCAP.
pub const WINDOWS_CEILING: PlatformCeiling = PlatformCeiling {
    platform: "windows",
    table: [
        (
            Guarantee::FsConfinement,
            Some("no openat2(RESOLVE_BENEATH); lexical fallback is not object-bound"),
        ),
        (
            Guarantee::ProcessConfinement,
            Some("AppContainer floor unbuilt; B1 unsupported here"),
        ),
        (
            Guarantee::NetworkConfinement,
            Some("no kernel-backed default-deny egress on this platform"),
        ),
        (Guarantee::EnvIsolation, None),
        (
            Guarantee::CredentialIsolation,
            Some("without fs/process confinement, ambient credentials are reachable"),
        ),
        (Guarantee::DisclosureFiltering, None),
        (Guarantee::FailClosedExecution, None),
    ],
};

/// Any platform we have no ceiling for: everything unsupported. The default
/// arm of the platform axis is the most restrictive — the exact opposite of a
/// `_ => true` fail-open.
pub const UNKNOWN_CEILING: PlatformCeiling = PlatformCeiling {
    platform: "unknown",
    table: [
        (Guarantee::FsConfinement, Some("unrecognized platform")),
        (Guarantee::ProcessConfinement, Some("unrecognized platform")),
        (Guarantee::NetworkConfinement, Some("unrecognized platform")),
        (Guarantee::EnvIsolation, Some("unrecognized platform")),
        (
            Guarantee::CredentialIsolation,
            Some("unrecognized platform"),
        ),
        (
            Guarantee::DisclosureFiltering,
            Some("unrecognized platform"),
        ),
        (
            Guarantee::FailClosedExecution,
            Some("unrecognized platform"),
        ),
    ],
};

/// The ceiling for the platform this binary was built for.
#[must_use]
pub fn current_platform_ceiling() -> &'static PlatformCeiling {
    #[cfg(target_os = "linux")]
    {
        &LINUX_CEILING
    }
    #[cfg(target_os = "macos")]
    {
        &MACOS_CEILING
    }
    #[cfg(windows)]
    {
        &WINDOWS_CEILING
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        &UNKNOWN_CEILING
    }
}

/// Verify object-bound workspace fs (`fs-canonical-containment`). On Linux the
/// `fs_cap::WorkspaceDir` capability resolves every access with
/// `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`; on kernels without
/// `openat2` the resolve *errors* (refusal, not downgrade). Elsewhere the
/// object-bound capability does not exist — the named open residual.
#[must_use]
pub fn verify_fs_object_bound() -> Verification {
    #[cfg(target_os = "linux")]
    {
        Verification::Verified {
            evidence: "fs_cap::WorkspaceDir: openat2(RESOLVE_BENEATH|NO_MAGICLINKS) object-bound \
                       resolve on every access; ENOSYS kernels refuse rather than degrade \
                       (register: fs-canonical-containment closed on Linux)"
                .into(),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Verification::Absent {
            deviation: "fs-canonical-containment",
            reason: "no openat2 on this platform; only the lexical-prefix fallback exists".into(),
        }
    }
}

/// Verify the mandatory confined executor (`p4-constrained-executor`): every
/// attacker-influenced subprocess is routed through one env-cleared, confined
/// spawn seam ([`crate::confined_exec::ConstrainedExecutor`] →
/// `agent_bridle::ConfinedCommand`, which `env_clear`s the child and applies the
/// kernel fs fence under an `AgentInfluenced` `Kernel` strength floor).
///
/// This is `Verified` iff the kernel fs fence the executor requires is actually
/// available on this host ([`crate::confined_exec::kernel_fs_fence_available`]) —
/// the runtime-checkable half of the guarantee, so the report can never claim
/// confinement the executor could not deliver. Where the fence is absent (a
/// pre-Landlock kernel, or any non-Linux platform) an `AgentInfluenced` spawn
/// *refuses* (`ExecRefused::ConfinementUnenforceable`) rather than run
/// unconfined — so `Absent` here is honest, not a silent downgrade.
///
/// The complementary half — that ALL attacker-influenced spawn sites route
/// through this seam (`agent-exec-todo-p4 == 0`) — is the CI-enforced
/// spawn-inventory gate (`scripts/spawn_inventory.py` over
/// `docs/security/spawn-inventory.toml`). The integration test
/// `constrained_executor_truth.rs` ties the two: it fails if the inventory
/// carries an unmigrated attacker spawn OR if this verifier disagrees with the
/// gate on a fence-available host — so live enforcement, this verifier, and the
/// register cannot drift apart.
#[must_use]
pub fn verify_constrained_executor() -> Verification {
    if crate::confined_exec::kernel_fs_fence_available() {
        Verification::Verified {
            evidence: "ConstrainedExecutor routes every attacker-influenced spawn through \
                       agent_bridle::ConfinedCommand under a Kernel strength floor: the child is \
                       env-cleared (only explicit grants) and the fs fence is kernel-enforced, \
                       fail-closed when it cannot be; the CI spawn-inventory gate holds \
                       agent-exec-todo-p4 at 0 so no attacker spawn bypasses it \
                       (constrained_executor_truth.rs ties the gate to this verifier)"
                .into(),
        }
    } else {
        Verification::Absent {
            deviation: "p4-constrained-executor",
            reason: "the kernel fs fence is unavailable on this host — an AgentInfluenced spawn \
                     refuses (fail-closed) rather than confine \
                     (see docs/security/spawn-inventory.toml)"
                .into(),
        }
    }
}

/// Verify fail-closed execution: a *required-but-unavailable* guarantee refuses
/// the operation instead of silently degrading. On Linux this is structural —
/// the `require()`/[`FailClosed`] gates plus the CI-enforced OCAP-DANGER/GATE
/// pairing (`scripts/ocap_check.py`) make un-gated dangerous paths a build
/// error. Off Linux, untrusted fs still falls back to lexical containment — a
/// silent downgrade — so this verifier stays honest-Absent there until the
/// refusal wiring lands.
#[must_use]
pub fn verify_fail_closed_execution() -> Verification {
    #[cfg(target_os = "linux")]
    {
        Verification::Verified {
            evidence: "require()/FailClosed capability gates; OCAP-DANGER/OCAP-GATE pairing \
                       statically enforced in CI (scripts/ocap_check.py); confined-by-default \
                       launch lane (resolve_lane: Yolo only via explicit --unsafe-host-exec)"
                .into(),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Verification::Absent {
            deviation: "fs-canonical-containment",
            reason: "untrusted fs falls back to lexical containment off Linux — a silent \
                     downgrade, not a refusal"
                .into(),
        }
    }
}

/// Verify **untrusted-child DIRECT network confinement** — the *narrow*,
/// provable property that an attacker-influenced child cannot create a DIRECT
/// off-box socket (`AF_INET` / `AF_INET6` / `AF_PACKET` → `EACCES`). This is the
/// basic network floor, split out from the stronger credential-bearing
/// [`verify_b1`]: every `AgentInfluenced` child runs under the seccomp
/// egress-deny floor by default (the `ConstrainedExecutor` `DenyAll` lane via
/// `newt-net-guard`; the `run_command` shell via agent-bridle 0.7.15
/// `ChildNetworkPolicy::DenyDirect`), and the spawn is REFUSED if the floor
/// cannot be established.
///
/// `Verified` iff both halves are enforceable on this host: the seccomp filter
/// ([`crate::netguard::egress_deny_supported`]) and the Landlock fs fence
/// ([`crate::confined_exec::kernel_fs_fence_available`]).
///
/// **This does NOT claim the child cannot reach the network at all.** The
/// seccomp floor deliberately allows `AF_UNIX`, and Landlock does not govern
/// unix-socket `connect`, so a confined child can still reach a host AF_UNIX
/// deputy (`af_unix_deputy.rs`) — an INDIRECT-egress residual tracked as the
/// ACTIVE `local-deputy-egress` deviation. This verifier asserts only the
/// direct-socket denial, and must not unlock credential-seeding (that needs
/// [`verify_b1`], which additionally requires the deferred mediated-egress
/// broker). Grounded by `net_guard_executor.rs` +
/// `run_command_child_under_net_none_cannot_open_a_socket_b1`.
#[must_use]
pub fn verify_network_confinement() -> Verification {
    #[cfg(target_os = "linux")]
    {
        if crate::netguard::egress_deny_supported()
            && crate::confined_exec::kernel_fs_fence_available()
        {
            Verification::Verified {
                evidence: "every live attacker-exec path denies direct egress by default, \
                           fail-closed: the ConstrainedExecutor callers (build_check / crew) run \
                           under NetGrant::DenyAll -> newt-net-guard seccomp socket() deny for \
                           AF_INET/AF_INET6/AF_PACKET (net_guard_executor.rs), and run_command's \
                           agent-bridle shell installs the same seccomp deny at the spawn owner via \
                           ChildNetworkPolicy::DenyDirect (agent-bridle 0.7.15), proven by \
                           run_command_child_under_net_none_cannot_open_a_socket_b1 — all beneath \
                           the Landlock fs fence. NARROW claim: no DIRECT off-box socket (AF_INET/\
                           AF_INET6/AF_PACKET) can be created on any path. NOT a no-egress claim — \
                           AF_UNIX local-deputy egress is a separate ACTIVE residual \
                           (local-deputy-egress)"
                    .into(),
            }
        } else {
            Verification::Absent {
                deviation: "b1-os-isolation",
                reason: "the seccomp egress floor or the Landlock fs fence is unavailable on this \
                         host — an AgentInfluenced spawn refuses rather than run with weaker \
                         network confinement"
                    .into(),
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Verification::Absent {
            deviation: "b1-os-isolation",
            reason: "no seccomp egress floor off Linux; the platform ceiling reports this \
                     unsupported"
                .into(),
        }
    }
}

/// The runtime half of the meet: the verifier results the report derives from.
/// Constructed from the SAME verifiers the capability gates call — there is no
/// field a caller can set to a free-form claim.
#[derive(Debug, Clone)]
pub struct RuntimeEvidence {
    /// The full credential-bearing OS-isolation floor (netns + mediated egress +
    /// …). Still open — gates `seed_live_credential` / `admit_untrusted_remote`.
    pub b1: Verification,
    pub disclosure: Verification,
    pub fs_object_bound: Verification,
    pub constrained_executor: Verification,
    pub fail_closed: Verification,
}

impl RuntimeEvidence {
    /// The live evidence for this build + session.
    #[must_use]
    pub fn current() -> Self {
        Self {
            b1: verify_b1(),
            disclosure: verify_disclosure_gate(),
            fs_object_bound: verify_fs_object_bound(),
            constrained_executor: verify_constrained_executor(),
            fail_closed: verify_fail_closed_execution(),
        }
    }

    /// meet: `Verified` only when both are; otherwise the first `Absent` wins.
    fn meet(a: &Verification, b: &Verification) -> Verification {
        match (a, b) {
            (Verification::Verified { evidence: ea }, Verification::Verified { evidence: eb }) => {
                Verification::Verified {
                    evidence: format!("{ea}; {eb}"),
                }
            }
            (Verification::Absent { .. }, _) => a.clone(),
            (_, Verification::Absent { .. }) => b.clone(),
        }
    }

    /// Which verifier(s) ground each guarantee. Compound guarantees take the
    /// meet — e.g. credential isolation needs BOTH the env-cleared executor
    /// (no inherited creds) and the B1 floor (no fs/net reach to stored creds).
    #[must_use]
    pub fn for_guarantee(&self, g: Guarantee) -> Verification {
        match g {
            Guarantee::FsConfinement => self.fs_object_bound.clone(),
            Guarantee::ProcessConfinement => Self::meet(&self.constrained_executor, &self.b1),
            // NetworkConfinement reflects the FULL floor (b1). The seccomp
            // egress-deny floor ([`verify_network_confinement`]) is real but
            // OPT-IN (`NetGrant::DenyAll`) and does not cover the agent-bridle
            // `run_command` model-exec path, so it does not by itself make the
            // whole-agent guarantee Enforced — we do not over-claim here.
            Guarantee::NetworkConfinement => self.b1.clone(),
            Guarantee::EnvIsolation => self.constrained_executor.clone(),
            Guarantee::CredentialIsolation => Self::meet(&self.constrained_executor, &self.b1),
            Guarantee::DisclosureFiltering => self.disclosure.clone(),
            Guarantee::FailClosedExecution => self.fail_closed.clone(),
        }
    }
}

/// The achieved-security report: platform + one honest [`Achieved`] entry per
/// guarantee. Posture surfaces render THIS (never independent strings), so
/// reporting cannot drift from enforcement.
#[derive(Debug, Clone)]
pub struct SecurityReport {
    pub platform: &'static str,
    entries: Vec<(Guarantee, Achieved)>,
}

impl SecurityReport {
    /// The report for this build's platform and the live runtime evidence.
    #[must_use]
    pub fn current() -> Self {
        Self::from_parts(current_platform_ceiling(), &RuntimeEvidence::current())
    }

    /// The pure meet — testable with any ceiling/evidence combination.
    #[must_use]
    pub fn from_parts(ceiling: &PlatformCeiling, ev: &RuntimeEvidence) -> Self {
        let entries = Guarantee::ALL
            .iter()
            .map(|&g| {
                let achieved = match ceiling.cannot_provide(g) {
                    // The ceiling never rounds up: unsupported stays unsupported
                    // even if a runtime verifier would report Verified (#12).
                    Some(reason) => Achieved::Unsupported {
                        reason: reason.into(),
                    },
                    None => match ev.for_guarantee(g) {
                        Verification::Verified { evidence } => Achieved::Enforced { evidence },
                        Verification::Absent { deviation, reason } => {
                            Achieved::Unverified { deviation, reason }
                        }
                    },
                };
                (g, achieved)
            })
            .collect();
        Self {
            platform: ceiling.platform,
            entries,
        }
    }

    /// The achieved state for one guarantee.
    #[must_use]
    pub fn achieved(&self, g: Guarantee) -> &Achieved {
        // ALL is exhaustive and from_parts builds one entry per guarantee.
        &self
            .entries
            .iter()
            .find(|(row, _)| *row == g)
            .expect("SecurityReport holds every Guarantee")
            .1
    }

    #[must_use]
    pub fn is_enforced(&self, g: Guarantee) -> bool {
        self.achieved(g).is_enforced()
    }

    /// One honest line per guarantee for posture banners / logs.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|(g, a)| match a {
                Achieved::Enforced { .. } => format!("{}: enforced", g.label()),
                Achieved::Unverified { deviation, .. } => {
                    format!("{}: OPEN ({deviation})", g.label())
                }
                Achieved::Unsupported { .. } => {
                    format!("{}: unsupported on {}", g.label(), self.platform)
                }
            })
            .collect()
    }
}

/// The refusal primitive for operations that require a guarantee: proceeds only
/// on `Enforced`, refuses (fail-closed) on `Unverified` and `Unsupported`. The
/// P8 law "if an operation requires an unavailable guarantee, refuse it".
pub fn require_achieved(report: &SecurityReport, g: Guarantee) -> Result<(), FailClosed> {
    match report.achieved(g) {
        Achieved::Enforced { .. } => Ok(()),
        Achieved::Unverified { deviation, reason } => Err(FailClosed {
            deviation,
            reason: reason.clone(),
        }),
        Achieved::Unsupported { reason } => Err(FailClosed {
            deviation: "platform-unsupported",
            reason: format!(
                "{} is unsupported on {}: {reason}",
                g.label(),
                report.platform
            ),
        }),
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    /// Evidence with everything Verified — for proving the ceiling still wins.
    fn all_verified() -> RuntimeEvidence {
        let v = || Verification::Verified {
            evidence: "synthetic".into(),
        };
        RuntimeEvidence {
            b1: v(),
            disclosure: v(),
            fs_object_bound: v(),
            constrained_executor: v(),
            fail_closed: v(),
        }
    }

    #[test]
    fn linux_report_matches_live_verifier_state() {
        // Reporting derives from the SAME verifiers the gates use (#11).
        // Disclosure (+ fs/fail-closed on Linux) report enforced; EnvIsolation and
        // NetworkConfinement track their own verifiers (Enforced where the kernel
        // fence is available); process / credential still name b1 (their meet
        // includes the open credential-bearing b1 half).
        // Disclosure filtering is now a LIVE, per-thread probe, so the report
        // tracks it in both directions rather than restating a constant.
        let bare = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
        assert!(
            matches!(
                bare.achieved(Guarantee::DisclosureFiltering),
                Achieved::Unverified {
                    deviation: "disclosure-gate-live-path",
                    ..
                }
            ),
            "no session filter on this thread ⇒ the row must be honest"
        );
        {
            let mut f = DisclosureFilter::new();
            f.register("sk-probe-9f3a2b7c1d4e6a8b");
            let _g = scoped_session_disclosure(f);
            let live = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
            assert!(matches!(
                live.achieved(Guarantee::DisclosureFiltering),
                Achieved::Enforced { .. }
            ));
        }
        let report = bare;
        // NetworkConfinement still names the full credential-bearing b1 floor
        // (the seccomp egress deny is opt-in and does not cover run_command).
        assert!(matches!(
            report.achieved(Guarantee::NetworkConfinement),
            Achieved::Unverified {
                deviation: "b1-os-isolation",
                ..
            }
        ));
        // EnvIsolation is the executor's own single-half guarantee: Enforced when
        // the kernel fence is available on this host, else fail-closed Absent.
        if crate::confined_exec::kernel_fs_fence_available() {
            assert!(matches!(
                report.achieved(Guarantee::EnvIsolation),
                Achieved::Enforced { .. }
            ));
        } else {
            assert!(matches!(
                report.achieved(Guarantee::EnvIsolation),
                Achieved::Unverified {
                    deviation: "p4-constrained-executor",
                    ..
                }
            ));
        }
        // Process + credential confinement take the meet with the still-open b1,
        // so they stay Unverified regardless of the executor half.
        assert!(matches!(
            report.achieved(Guarantee::ProcessConfinement),
            Achieved::Unverified { .. }
        ));
        assert!(matches!(
            report.achieved(Guarantee::CredentialIsolation),
            Achieved::Unverified { .. }
        ));
    }

    #[test]
    fn ceiling_never_rounds_up() {
        // Adversarial #12: even with EVERY runtime verifier Verified, a
        // platform whose ceiling says "cannot provide" must report Unsupported
        // — an unsupported platform can never claim Linux-equivalent OCAP.
        for ceiling in [&MACOS_CEILING, &WINDOWS_CEILING] {
            let report = SecurityReport::from_parts(ceiling, &all_verified());
            for g in [
                Guarantee::FsConfinement,
                Guarantee::ProcessConfinement,
                Guarantee::NetworkConfinement,
                Guarantee::CredentialIsolation,
            ] {
                assert!(
                    matches!(report.achieved(g), Achieved::Unsupported { .. }),
                    "{} must be Unsupported on {}",
                    g.label(),
                    ceiling.platform
                );
            }
        }
    }

    #[test]
    fn unknown_platform_is_fully_unsupported() {
        // The default arm of the platform axis is the MOST restrictive.
        let report = SecurityReport::from_parts(&UNKNOWN_CEILING, &all_verified());
        for g in Guarantee::ALL {
            assert!(
                matches!(report.achieved(g), Achieved::Unsupported { .. }),
                "{} must be Unsupported on unknown platforms",
                g.label()
            );
        }
    }

    #[test]
    fn require_achieved_refuses_unverified_and_unsupported() {
        // The refusal primitive: Enforced proceeds; everything else refuses.
        // Mock the evidence (like `compound_guarantees_take_the_meet`) rather than
        // the live `RuntimeEvidence::current()` host probe: `verify_constrained_executor`
        // is now honest (Verified only when `kernel_fs_fence_available()`), so off
        // Linux the `meet(constrained_executor, b1)` half flips to
        // `p4-constrained-executor` and this deviation assertion would be
        // platform-dependent. With `constrained_executor` synthetically Verified,
        // b1 is the only Absent half, so the deviation is `b1` everywhere.
        let mut ev = all_verified();
        ev.b1 = verify_b1();
        let linux = SecurityReport::from_parts(&LINUX_CEILING, &ev);
        assert!(require_achieved(&linux, Guarantee::DisclosureFiltering).is_ok());
        let err = require_achieved(&linux, Guarantee::CredentialIsolation).unwrap_err();
        assert_eq!(err.deviation, "b1-os-isolation");

        let mac = SecurityReport::from_parts(&MACOS_CEILING, &all_verified());
        let err = require_achieved(&mac, Guarantee::FsConfinement).unwrap_err();
        assert_eq!(err.deviation, "platform-unsupported");
        assert!(err.to_string().contains("fail-closed"));
    }

    #[test]
    fn compound_guarantees_take_the_meet() {
        // Credential isolation needs BOTH the executor and b1: one Absent half
        // keeps the guarantee Unverified even when the other half is Verified.
        let mut ev = all_verified();
        ev.b1 = verify_b1(); // Absent
        let report = SecurityReport::from_parts(&LINUX_CEILING, &ev);
        assert!(matches!(
            report.achieved(Guarantee::CredentialIsolation),
            Achieved::Unverified {
                deviation: "b1-os-isolation",
                ..
            }
        ));
        // And with both halves Verified it is enforced.
        let report = SecurityReport::from_parts(&LINUX_CEILING, &all_verified());
        assert!(report.is_enforced(Guarantee::CredentialIsolation));
    }

    #[test]
    fn summary_lines_cover_every_guarantee_honestly() {
        let report = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
        let lines = report.summary_lines();
        assert_eq!(lines.len(), Guarantee::ALL.len());
        // Honest means honest in both directions: outside a live turn there is
        // no thread-local backstop, and the summary says so rather than
        // reporting a filter that is not there.
        assert!(lines
            .iter()
            .any(|l| l.contains("disclosure-filtering: OPEN (disclosure-gate-live-path)")));
        {
            let mut f = DisclosureFilter::new();
            f.register("sk-probe-9f3a2b7c1d4e6a8b");
            let _g = scoped_session_disclosure(f);
            let live = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
            assert!(live
                .summary_lines()
                .iter()
                .any(|l| l == "disclosure-filtering: enforced"));
        }
        // Network confinement still names the full credential-bearing b1 floor
        // (the seccomp egress deny is opt-in and does not cover run_command).
        assert!(lines
            .iter()
            .any(|l| l.contains("network-confinement: OPEN (b1-os-isolation)")));
    }

    #[test]
    fn current_report_reflects_build_platform() {
        // On the Linux CI/dev platform the live report enforces fs +
        // fail-closed; on non-Linux builds those same rows must be honest
        // (Absent/Unsupported), never a silent Linux-equivalent claim.
        let report = SecurityReport::current();
        #[cfg(target_os = "linux")]
        {
            assert_eq!(report.platform, "linux");
            assert!(report.is_enforced(Guarantee::FsConfinement));
            assert!(report.is_enforced(Guarantee::FailClosedExecution));
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_ne!(report.platform, "linux");
            assert!(!report.is_enforced(Guarantee::FsConfinement));
            assert!(!report.is_enforced(Guarantee::FailClosedExecution));
        }
        // Everywhere: disclosure filtering is a per-THREAD probe, not a
        // process-wide constant. `SecurityReport::current()` runs on a thread
        // with no session filter installed, so the honest answer is that the
        // backstop is not in effect here.
        assert!(!report.is_enforced(Guarantee::DisclosureFiltering));
    }
}

#[cfg(test)]
mod sod_tests {
    use super::*;

    #[test]
    fn distinctness_primitive() {
        assert!(proposer_distinct("SHA256:proposer", "SHA256:worker"));
        assert!(!proposer_distinct("SHA256:same", "SHA256:same"));
        assert!(
            !proposer_distinct("", "SHA256:worker"),
            "empty proposer is not distinct"
        );
    }

    #[test]
    fn verify_sod_is_absent_until_taint_aware() {
        // Even with distinct keys, sod stays open (taint-aware half unbuilt).
        let v = verify_sod("SHA256:proposer", "SHA256:worker");
        assert!(!v.is_verified());
        assert_eq!(v.deviation(), Some("sod-proposer-not-worker"));
        // Self-proposal reports the distinctness failure specifically.
        let self_prop = verify_sod("SHA256:same", "SHA256:same");
        assert!(
            matches!(self_prop, Verification::Absent { reason, .. } if reason.contains("self-proposal"))
        );
    }

    #[test]
    fn auto_apply_fails_closed_both_ways() {
        assert!(matches!(
            auto_apply_policy("SHA256:p", "SHA256:w"),
            Err(FailClosed {
                deviation: "sod-proposer-not-worker",
                ..
            })
        ));
        assert!(auto_apply_policy("SHA256:same", "SHA256:same").is_err());
    }
}
