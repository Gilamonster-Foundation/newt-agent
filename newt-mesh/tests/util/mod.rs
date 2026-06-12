//! Shared timing helper for the mesh integration tests (issue #274).
//!
//! The agent-mesh bus resolves a peer fingerprint via mDNS with a
//! fixed internal window (`RESOLVE_TIMEOUT`, 5s as of agent-mesh
//! 0.6.x) and fails with `BusError::Unreachable("peer … not announced
//! within …")` when the announce hasn't propagated yet. On shared CI
//! runners mDNS propagation occasionally takes longer than that
//! window, which made these tests flake with a fixed pre-ask sleep.
//!
//! [`with_announce_grace`] replaces the fixed sleep with
//! poll-with-deadline: run the first-contact operation, and retry it
//! **only** when it failed because the peer wasn't announced yet,
//! until [`ANNOUNCE_DEADLINE`]. Each attempt's internal resolve is
//! event-driven, so the test passes the moment the announce lands —
//! there is no fixed cost on fast machines. The honest signal is
//! preserved: a peer that never announces still fails (with the real
//! resolver error) once the deadline is exhausted, and every *other*
//! error — wire-shape mismatch, handler failure, request timeout —
//! propagates immediately, never retried.
//!
//! Grace is necessary but not sufficient (issue #305): every `Bus`
//! spawns its own mDNS `ServiceDaemon`s, so concurrently-running
//! tests put many daemons in one process — all members of the same
//! port-5353 SO_REUSEPORT group. Multicast re-announces reach every
//! member, but *unicast* mDNS responses are delivered to ONE socket
//! in the group, so a specific browser can stay blind to a specific
//! peer for the full [`ANNOUNCE_DEADLINE`] while its in-process
//! siblings resolve instantly — and retrying on the same bus retries
//! the same blind daemon. The mesh-binding tests therefore carry
//! `#[serial_test::serial(mesh_mdns)]`: one client/responder pair at
//! a time, which is also the topology the contract actually models.
//! If a *serialized* run still reports "not announced within …" for
//! the whole deadline, do not widen the deadline — that is a real
//! agent-mesh discovery bug; take it upstream.

use std::future::Future;
use std::time::{Duration, Instant};

use agent_mesh_bus::BusError;

/// Total ceiling for announce propagation. Generous on purpose: it is
/// only ever reached when the announce is genuinely missing (test
/// fails) or the runner is pathologically slow (test still passes,
/// late). A healthy run resolves on the first attempt.
pub const ANNOUNCE_DEADLINE: Duration = Duration::from_secs(30);

/// Pause between attempts once one resolve window has come up empty.
pub const ANNOUNCE_RETRY_PAUSE: Duration = Duration::from_millis(250);

/// `true` iff `e` is the bus's "peer not announced within …" resolve
/// failure — the only transient, self-resolving error in first
/// contact. Everything else (announced-without-pubkey, dial failures,
/// timeouts) is a real bug and must fail the test on the spot.
pub fn bus_unannounced(e: &BusError) -> bool {
    matches!(e, BusError::Unreachable(msg) if msg.contains("not announced within"))
}

/// Run `op` (a first-contact request whose initial step is resolving
/// the peer over mDNS), retrying only while `is_unannounced(&err)`
/// holds and [`ANNOUNCE_DEADLINE`] has not passed. Returns the first
/// success, the first non-announce error, or the final announce error
/// once the deadline is exhausted.
pub async fn with_announce_grace<T, E, F, Fut, P>(mut op: F, is_unannounced: P) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    P: Fn(&E) -> bool,
    E: std::fmt::Display,
{
    let deadline = Instant::now() + ANNOUNCE_DEADLINE;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if is_unannounced(&e) && Instant::now() < deadline => {
                // Keep the flake visible in test output even when the
                // retry absorbs it — silent retries stop being signal.
                eprintln!("announce not yet visible ({e}); retrying in {ANNOUNCE_RETRY_PAUSE:?}");
                tokio::time::sleep(ANNOUNCE_RETRY_PAUSE).await;
            }
            Err(e) => return Err(e),
        }
    }
}
