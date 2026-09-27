//! uplink — the docked host dials out to its hub (`docs/decisions/newt_web_docking.md`
//! K8, the interim long-poll carrier of K8.3).
//!
//! A docked host binds an **outbound-only** bus (agent-mesh
//! `Bus::bind_outbound_only`: every connection it did not dial is refused) and
//! keeps one request outstanding at its hub on [`DOCK_UPLINK_TOPIC`]. The hub
//! holds that poll until it has a [`DockRequest`] for this host, then answers
//! the poll with it; the host serves it and carries the [`DockReply`] on its
//! next poll. The reply to each poll returns on the connection the poll went
//! out on, so the hub never dials the host and the host never needs to be
//! reachable.
//!
//! Direction is not authority (K8.2): the hub is still the requester and the
//! host the authorizer. The hub's identity is the pubkey the host dialed, and
//! the bus only delivers a poll's reply signed by that key; the host checks it
//! against its own dock registry on every request ([`crate::dock::serve_dock`],
//! the same path as the direct responder).
//!
//! Replaced, not extended, when agent-mesh ships `session_streams` (K8-f).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_mesh_bus::{Bus, PeerEndpoint, RequestContext, Topic};
use agent_mesh_core::{AgentKey, Fingerprint, UserKey};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::dock::{serve_dock, DockReply, DockRequest};

/// Topic (under the operator's user namespace) a docked host polls its hub on.
pub const DOCK_UPLINK_TOPIC: &str = "newt/dock/uplink/v1";

/// How long the hub holds a poll that has no request for its host.
const HOLD: Duration = Duration::from_secs(20);
/// How long a host waits for its poll's reply: the hold plus headroom.
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a hub request waits for the host's answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconnect backoff bounds after a failed poll.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// One poll from a host: the answer to the previous job, if any.
#[derive(Debug, Serialize, Deserialize)]
struct Poll {
    answer: Option<(u64, DockReply)>,
}

/// The hub's reply to a poll: the next job for this host, if any.
#[derive(Debug, Serialize, Deserialize)]
struct Work {
    job: Option<(u64, DockRequest)>,
}

// ── Host side ────────────────────────────────────────────────────────────────

/// A docked host's uplink to one hub. Dropping it (or [`Self::close`]) closes
/// the uplink.
pub struct DockUplink {
    task: JoinHandle<()>,
    local_port: u16,
}

impl DockUplink {
    /// Open an uplink from this host (`agent`, serving the store at
    /// `state_dir`) to `hub`, and keep it open, reconnecting with jittered
    /// backoff after a failure. The hub is identified by `hub`'s pubkey — the
    /// one approved at the ceremony — never by its address.
    ///
    /// # Errors
    /// The outbound-only bus failed to bind.
    pub async fn start(
        user: &UserKey,
        agent: AgentKey,
        state_dir: PathBuf,
        hub: PeerEndpoint,
    ) -> anyhow::Result<Self> {
        let topic = Topic::new(user.fingerprint(), DOCK_UPLINK_TOPIC);
        let bus = Bus::bind_outbound_only(user, agent).await?;
        let local_port = bus.local_port();
        let task = tokio::spawn(run(bus, topic, state_dir, hub));
        Ok(Self { task, local_port })
    }

    /// The UDP port the uplink dials from (it accepts nothing on it).
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Close the uplink.
    pub fn close(self) {}
}

impl Drop for DockUplink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(bus: Bus, topic: Topic, state_dir: PathBuf, hub: PeerEndpoint) {
    // The reply to a poll is signed by the hub's key, or the bus drops it.
    let hub_fp = hub.fingerprint().hex();
    let mut answer: Option<(u64, DockReply)> = None;
    let mut backoff = BACKOFF_MIN;
    loop {
        let poll = Poll {
            answer: answer.clone(),
        };
        let body = serde_json::to_vec(&poll).unwrap_or_default();
        match bus.request_direct(hub, &topic, body, POLL_TIMEOUT).await {
            Ok(reply) => {
                answer = None;
                backoff = BACKOFF_MIN;
                match serde_json::from_slice::<Work>(&reply) {
                    Ok(Work {
                        job: Some((id, request)),
                    }) => {
                        let reply =
                            serve_dock(state_dir.clone(), hub_fp.clone(), Ok(request)).await;
                        answer = Some((id, reply));
                    }
                    Ok(Work { job: None }) => {}
                    Err(e) => tracing::warn!(error = %e, "dock uplink: unreadable work from hub"),
                }
            }
            Err(e) => {
                // Keep `answer` to resend on the next poll.
                tracing::debug!(error = %e, ?backoff, "dock uplink: poll failed; retrying");
                tokio::time::sleep(jittered(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// `d` scaled into `[d/2, d)`, so many hosts do not reconnect in lockstep.
fn jittered(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.subsec_nanos());
    d / 2 + d.mul_f64(f64::from(nanos % 1000) / 2000.0)
}

// ── Hub side ─────────────────────────────────────────────────────────────────

/// A hub's view of the hosts uplinked to it.
#[derive(Default)]
pub(crate) struct UplinkHub {
    hosts: Mutex<HashMap<Fingerprint, Arc<HostLink>>>,
    next_id: AtomicU64,
}

/// One uplinked host: its queued jobs and the answers still awaited.
struct HostLink {
    jobs_tx: mpsc::UnboundedSender<(u64, DockRequest)>,
    jobs_rx: AsyncMutex<mpsc::UnboundedReceiver<(u64, DockRequest)>>,
    awaiting: Mutex<HashMap<u64, oneshot::Sender<DockReply>>>,
    last_poll: Mutex<Instant>,
}

impl HostLink {
    fn new() -> Self {
        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
        Self {
            jobs_tx,
            jobs_rx: AsyncMutex::new(jobs_rx),
            awaiting: Mutex::default(),
            last_poll: Mutex::new(Instant::now()),
        }
    }
}

impl UplinkHub {
    fn link(&self, host: Fingerprint) -> Arc<HostLink> {
        let mut hosts = self.hosts.lock().unwrap();
        hosts
            .entry(host)
            .or_insert_with(|| Arc::new(HostLink::new()))
            .clone()
    }

    /// Answer one poll from `host`: deliver the answer it carries, then hold
    /// until there is a job for it or [`HOLD`] lapses.
    async fn poll(&self, host: Fingerprint, poll: Poll) -> Work {
        let link = self.link(host);
        *link.last_poll.lock().unwrap() = Instant::now();
        if let Some((id, reply)) = poll.answer {
            if let Some(tx) = link.awaiting.lock().unwrap().remove(&id) {
                let _ = tx.send(reply);
            }
        }
        let mut jobs = link.jobs_rx.lock().await;
        Work {
            job: tokio::time::timeout(HOLD, jobs.recv()).await.ok().flatten(),
        }
    }

    /// Hand `request` to `host` over its uplink and wait for the answer.
    pub(crate) async fn request(
        &self,
        host: Fingerprint,
        request: DockRequest,
    ) -> anyhow::Result<DockReply> {
        let link = self.hosts.lock().unwrap().get(&host).cloned();
        let link = link
            .filter(|l| l.last_poll.lock().unwrap().elapsed() < POLL_TIMEOUT)
            .ok_or_else(|| anyhow::anyhow!("host {} has no open uplink", host.short()))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        link.awaiting.lock().unwrap().insert(id, tx);
        let _ = link.jobs_tx.send((id, request));
        let answer = tokio::time::timeout(ANSWER_TIMEOUT, rx).await;
        link.awaiting.lock().unwrap().remove(&id);
        match answer {
            Ok(Ok(reply)) => Ok(reply),
            _ => Err(anyhow::anyhow!(
                "host {} did not answer over its uplink",
                host.short()
            )),
        }
    }
}

/// Serve uplink polls on `bus` into `hub`. The poller is the verified envelope
/// signer, so a host can only take jobs addressed to its own fingerprint.
pub(crate) fn serve(bus: &Bus, user_fp: Fingerprint, hub: Arc<UplinkHub>) {
    let topic = Topic::new(user_fp, DOCK_UPLINK_TOPIC);
    bus.handle_requests_with_context(topic, move |ctx: RequestContext, body| {
        let hub = hub.clone();
        async move {
            let poll: Poll = serde_json::from_slice(&body)?;
            let work = hub.poll(ctx.caller_agent_fp, poll).await;
            Ok(serde_json::to_vec(&work)?)
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(byte: u8) -> Fingerprint {
        Fingerprint([byte; 32])
    }

    #[tokio::test]
    async fn a_request_to_a_host_with_no_uplink_fails_at_once() {
        let hub = UplinkHub::default();
        let err = hub
            .request(fp(1), DockRequest::ListSessions)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no open uplink"), "{err}");
    }

    /// A job reaches the polling host, and its answer — carried on the host's
    /// next poll — completes the hub's request.
    #[tokio::test(start_paused = true)]
    async fn a_job_rides_a_held_poll_and_its_answer_rides_the_next() {
        let hub = Arc::new(UplinkHub::default());
        let held = tokio::spawn({
            let hub = hub.clone();
            async move { hub.poll(fp(1), Poll { answer: None }).await }
        });
        tokio::task::yield_now().await;
        let asking = tokio::spawn({
            let hub = hub.clone();
            async move { hub.request(fp(1), DockRequest::ListSessions).await }
        });
        let Work {
            job: Some((id, DockRequest::ListSessions)),
        } = held.await.unwrap()
        else {
            panic!("the held poll carries the job");
        };
        let next = tokio::spawn({
            let hub = hub.clone();
            async move {
                let answer = Some((id, DockReply::Sessions(Vec::new())));
                hub.poll(fp(1), Poll { answer }).await
            }
        });
        assert!(matches!(asking.await.unwrap(), Ok(DockReply::Sessions(s)) if s.is_empty()));
        assert!(
            next.await.unwrap().job.is_none(),
            "no further work: the hold lapses"
        );
    }

    /// A host only ever receives jobs addressed to its own fingerprint (the
    /// poller is the verified envelope signer).
    #[tokio::test(start_paused = true)]
    async fn a_host_never_receives_another_hosts_job() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let asking = tokio::spawn({
            let hub = hub.clone();
            async move { hub.request(fp(1), DockRequest::ListSessions).await }
        });
        tokio::task::yield_now().await;
        assert!(hub.poll(fp(2), Poll { answer: None }).await.job.is_none());
        assert!(
            asking.await.unwrap().is_err(),
            "host 1 never polled for it, so the request times out"
        );
    }
}
