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
//! Delivery is at most once per request, never exactly once:
//!
//! - **Before dispatch** (a poll taking the request), a request that expires or
//!   whose requester goes away is withdrawn and never sent.
//! - **After dispatch**, a timeout means the outcome is unknown: the host may
//!   have run it. Nothing rolls it back, and nothing re-sends it — an `Inject`
//!   retried by a caller is a new operation.
//! - A host retries only its *answer*, on its next poll, until a poll
//!   succeeds. Jobs carry 128-bit random ids, so an answer retained across a
//!   hub restart matches nothing on the new hub.
//! - [`DockUplink::close`] stops polling and closes the bus, but lets a request
//!   already being served finish; its answer is discarded.
//!
//! **Admission (K8.5).** A hub serves only a host it has promoted — approved
//! in its own signed dock registry. Each poll carries the host's key and
//! instance name; the key must be the poll's verified signer. An unpromoted
//! host is staged for `newt dock approve --staged` on the hub's terminal and is
//! told so ([`UplinkState::Staged`]); it gets no request.
//!
//! Jobs and answers are never persisted; only a staged host's key and name
//! are, on the hub, until approved or expired. Replaced, not extended, when
//! agent-mesh ships `session_streams` (K8-f).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_mesh_bus::{Bus, CorrelationId, PeerEndpoint, RequestContext, Topic};
use agent_mesh_core::{Fingerprint, UserKey};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::dock::{dock_agent, serve_dock, DockReply, DockRequest, DockRole};

/// Topic (under the operator's user namespace) a docked host polls its hub on.
pub const DOCK_UPLINK_TOPIC: &str = "newt/dock/uplink/v1";

/// How long the hub holds a poll that has no request for its host.
const HOLD: Duration = Duration::from_secs(20);
/// How long the hub holds a poll from a host it has only staged: short, so an
/// approval takes effect promptly, but long enough that the host does not spin.
const STAGED_HOLD: Duration = Duration::from_secs(5);
/// How long a host waits for its poll's reply: the hold plus headroom.
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a hub request waits for the host's answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconnect backoff bounds after a failed poll.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Who is polling: the host's dock key, which the hub checks against the
/// poll's verified signer, and its instance name, which is display only. A hub
/// stages a host it has not approved under these (K8.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub(crate) pubkey: [u8; 32],
    pub(crate) instance: String,
}

/// One poll from a host: who it is, and the answer to the previous job, if any.
#[derive(Debug, Serialize, Deserialize)]
struct Poll {
    host: Hello,
    answer: Option<(JobId, DockReply)>,
}

/// The hub's reply to a poll: the next job for this host, if any, or that the
/// hub has only staged this host and will send it none until it is approved.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Work {
    job: Option<(JobId, DockRequest)>,
    #[serde(default)]
    staged: bool,
}

// ── Host side ────────────────────────────────────────────────────────────────

/// What a [`DockUplink`] is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkState {
    /// A poll is outstanding at the hub.
    Polling,
    /// Serving a request the hub handed over.
    Serving,
    /// The last poll failed; waiting to retry.
    Backoff,
    /// The hub has not approved this host yet: it has staged it for
    /// `newt dock approve --staged` on the hub's terminal. Polling continues.
    Staged,
    /// Stopped: no further polls, and the bus is closed.
    Closed,
}

/// A docked host's uplink to one hub.
///
/// **Shutdown.** [`Self::close`] stops polling, closes the uplink's bus, and
/// waits for the runner to finish; closing the bus closes its endpoint, so the
/// UDP socket is released promptly, even with a poll's reply window open. A
/// dock request the host is already serving is not interrupted: it runs to
/// completion (an `Inject` it has started still lands) and its answer is
/// discarded, so the hub sees that request time out with an unknown outcome. Dropping a `DockUplink` without `close` only
/// signals the runner: it stops at its next await point and closes the bus
/// itself while the runtime is alive, but nothing waits for that.
pub struct DockUplink {
    stop: Option<oneshot::Sender<()>>,
    runner: Option<JoinHandle<()>>,
    state: watch::Receiver<UplinkState>,
    local_port: u16,
}

impl DockUplink {
    /// Open an uplink from this host (dock `instance`, serving the store at
    /// `state_dir`) to `hub`, and keep it open, reconnecting with jittered
    /// backoff after a failure. The host's dock key is derived here from
    /// `instance`, so the name it reports always matches the key it signs
    /// with. The hub is identified by `hub`'s pubkey — the one approved at the
    /// ceremony — never by its address.
    ///
    /// # Errors
    /// The outbound-only bus failed to bind.
    pub async fn start(
        user: &UserKey,
        instance: &str,
        state_dir: PathBuf,
        hub: PeerEndpoint,
    ) -> anyhow::Result<Self> {
        let topic = Topic::new(user.fingerprint(), DOCK_UPLINK_TOPIC);
        let agent = dock_agent(user, DockRole::Host, instance);
        let hello = Hello {
            pubkey: agent.public_bytes(),
            instance: instance.to_owned(),
        };
        let bus = Bus::bind_outbound_only(user, agent).await?;
        let local_port = bus.local_port();
        let (stop, stopped) = oneshot::channel();
        let (state_tx, state) = watch::channel(UplinkState::Polling);
        let runner = tokio::spawn(run(bus, topic, hello, state_dir, hub, stopped, state_tx));
        Ok(Self {
            stop: Some(stop),
            runner: Some(runner),
            state,
            local_port,
        })
    }

    /// The UDP port the uplink dials from (it accepts nothing on it).
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// What the uplink is doing now.
    #[must_use]
    pub fn state(&self) -> UplinkState {
        *self.state.borrow()
    }

    /// Wait until the uplink reaches `want`.
    #[cfg(test)]
    pub(crate) async fn reached(&mut self, want: UplinkState) {
        let _ = self.state.wait_for(|s| *s == want).await;
    }

    /// Stop polling, close the bus, and wait for the runner to finish. See the
    /// shutdown notes on [`DockUplink`].
    pub async fn close(mut self) {
        drop(self.stop.take());
        if let Some(runner) = self.runner.take() {
            let _ = runner.await;
        }
    }
}

impl Drop for DockUplink {
    fn drop(&mut self) {
        // Best effort: dropping `stop` signals the runner (see `close`).
        drop(self.stop.take());
    }
}

async fn run(
    bus: Bus,
    topic: Topic,
    hello: Hello,
    state_dir: PathBuf,
    hub: PeerEndpoint,
    mut stopped: oneshot::Receiver<()>,
    state: watch::Sender<UplinkState>,
) {
    // The reply to a poll is signed by the hub's key, or the bus drops it.
    let hub_fp = hub.fingerprint().hex();
    let mut answer: Option<(JobId, DockReply)> = None;
    let mut backoff = BACKOFF_MIN;
    let mut staged = false;
    loop {
        state.send_replace(if staged {
            UplinkState::Staged
        } else {
            UplinkState::Polling
        });
        let poll = Poll {
            host: hello.clone(),
            answer: answer.clone(),
        };
        let body = serde_json::to_vec(&poll).unwrap_or_default();
        let polled = tokio::select! {
            biased;
            _ = &mut stopped => break,
            polled = bus.request_direct(hub, &topic, body, POLL_TIMEOUT) => polled,
        };
        match polled {
            Ok(reply) => {
                answer = None;
                backoff = BACKOFF_MIN;
                let work = serde_json::from_slice::<Work>(&reply);
                staged = matches!(work, Ok(Work { staged: true, .. }));
                match work {
                    Ok(Work {
                        job: Some((id, request)),
                        ..
                    }) => {
                        // Not raced against `stopped`: a started operation
                        // runs to completion.
                        state.send_replace(UplinkState::Serving);
                        let reply =
                            serve_dock(state_dir.clone(), hub_fp.clone(), Ok(request)).await;
                        answer = Some((id, reply));
                    }
                    Ok(Work { job: None, .. }) => {}
                    Err(e) => tracing::warn!(error = %e, "dock uplink: unreadable work from hub"),
                }
            }
            Err(e) => {
                // Keep `answer` to resend on the next poll.
                tracing::debug!(error = %e, ?backoff, "dock uplink: poll failed; retrying");
                state.send_replace(UplinkState::Backoff);
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    () = tokio::time::sleep(jittered(backoff)) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
    if let Err(e) = bus.close().await {
        tracing::debug!(error = %e, "dock uplink: bus close failed");
    }
    state.send_replace(UplinkState::Closed);
}

/// `d` scaled into `[d/2, d)`, so many hosts do not reconnect in lockstep.
fn jittered(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.subsec_nanos());
    d / 2 + d.mul_f64(f64::from(nanos % 1000) / 2000.0)
}

// ── Hub side ─────────────────────────────────────────────────────────────────

/// Most requests a hub keeps outstanding (queued or dispatched) for one host.
/// A request past it fails at once rather than growing the queue.
const MAX_OUTSTANDING: usize = 64;

/// A job's identity: 128 random bits, so an answer a host retained across a
/// hub restart can never name a job the new hub issued.
type JobId = [u8; 16];

/// A request waiting to be dispatched to its host.
struct Job {
    id: JobId,
    request: DockRequest,
    deadline: Instant,
    answer: oneshot::Sender<DockReply>,
}

/// One host's outstanding work, guarded by a single lock so the queued →
/// dispatched transition and a requester's withdrawal are linearized.
#[derive(Default)]
struct HostWork {
    queued: VecDeque<Job>,
    dispatched: HashMap<JobId, oneshot::Sender<DockReply>>,
}

impl HostWork {
    /// Drop work nobody can receive: expired, or its requester has gone.
    fn prune(&mut self, now: Instant) {
        self.queued
            .retain(|job| now < job.deadline && !job.answer.is_closed());
        self.dispatched.retain(|_, answer| !answer.is_closed());
    }

    /// Take the next live job and mark it dispatched — the point after which
    /// its outcome belongs to the host.
    fn dispatch(&mut self, now: Instant) -> Option<(JobId, DockRequest)> {
        self.prune(now);
        let job = self.queued.pop_front()?;
        self.dispatched.insert(job.id, job.answer);
        Some((job.id, job.request))
    }
}

/// One uplinked host.
struct HostLink {
    work: Mutex<HostWork>,
    queued: Notify,
    last_poll: Mutex<Instant>,
}

impl HostLink {
    fn new() -> Self {
        Self {
            work: Mutex::default(),
            queued: Notify::new(),
            last_poll: Mutex::new(Instant::now()),
        }
    }
}

/// Withdraws a request's job on every exit — answer, timeout, or the
/// requester being dropped. If the job is still queued it is never sent; if
/// it was already dispatched, withdrawing only forgets the answer.
struct Withdraw {
    link: Arc<HostLink>,
    id: JobId,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        let mut work = self.link.work.lock().unwrap();
        work.queued.retain(|job| job.id != self.id);
        work.dispatched.remove(&self.id);
    }
}

/// A hub's view of the hosts uplinked to it.
///
/// **Dispatch boundary.** A request is *dispatched* when a poll takes it into
/// the host's reply. Before that, a request that expires, is cancelled, or is
/// dropped is never sent. After it, a timeout means the outcome is unknown:
/// the host may have run it (an `Inject` may have been enqueued), and nothing
/// here rolls it back or retries it.
#[derive(Default)]
pub(crate) struct UplinkHub {
    hosts: Mutex<HashMap<Fingerprint, Arc<HostLink>>>,
    polled: Notify,
}

impl UplinkHub {
    fn link(&self, host: Fingerprint) -> Arc<HostLink> {
        let mut hosts = self.hosts.lock().unwrap();
        hosts
            .entry(host)
            .or_insert_with(|| Arc::new(HostLink::new()))
            .clone()
    }

    fn live_link(&self, host: Fingerprint) -> Option<Arc<HostLink>> {
        let link = self.hosts.lock().unwrap().get(&host).cloned()?;
        let fresh = link.last_poll.lock().unwrap().elapsed() < POLL_TIMEOUT;
        fresh.then_some(link)
    }

    /// Wait until `host` has polled recently enough to take requests.
    #[cfg(test)]
    pub(crate) async fn wait_for_uplink(&self, host: Fingerprint) {
        loop {
            let polled = self.polled.notified();
            if self.live_link(host).is_some() {
                return;
            }
            polled.await;
        }
    }

    /// Answer one poll from `host`: deliver the answer it carries, then hold
    /// until there is a live job for it or [`HOLD`] lapses. An answer is
    /// matched only within this host's own dispatched work.
    async fn poll(&self, host: Fingerprint, poll: Poll) -> Work {
        let link = self.link(host);
        *link.last_poll.lock().unwrap() = Instant::now();
        self.polled.notify_waiters();
        if let Some((id, reply)) = poll.answer {
            let answer = link.work.lock().unwrap().dispatched.remove(&id);
            if let Some(answer) = answer {
                let _ = answer.send(reply);
            }
        }
        let hold = Instant::now() + HOLD;
        loop {
            let queued = link.queued.notified();
            let job = link.work.lock().unwrap().dispatch(Instant::now());
            if job.is_some() {
                return Work { job, staged: false };
            }
            if tokio::time::timeout_at(hold, queued).await.is_err() {
                return Work::default();
            }
        }
    }

    /// Answer a poll from a host this hub has not approved: drop any link it
    /// held (a revoked host loses its uplink here), hold the poll so the host
    /// does not spin, and tell it it is only staged. It gets no job.
    async fn stage(&self, host: Fingerprint) -> Work {
        self.hosts.lock().unwrap().remove(&host);
        tokio::time::sleep(STAGED_HOLD).await;
        Work {
            job: None,
            staged: true,
        }
    }

    /// Hand `request` to `host` over its uplink and wait for the answer, until
    /// [`ANSWER_TIMEOUT`]. See the dispatch boundary on [`UplinkHub`].
    pub(crate) async fn request(
        &self,
        host: Fingerprint,
        request: DockRequest,
    ) -> anyhow::Result<DockReply> {
        let link = self
            .live_link(host)
            .ok_or_else(|| anyhow::anyhow!("host {} has no open uplink", host.short()))?;
        let now = Instant::now();
        let deadline = now + ANSWER_TIMEOUT;
        let id = CorrelationId::new_random().0;
        let (answer, answered) = oneshot::channel();
        {
            let mut work = link.work.lock().unwrap();
            work.prune(now);
            if work.queued.len() + work.dispatched.len() >= MAX_OUTSTANDING {
                anyhow::bail!(
                    "host {} already has {MAX_OUTSTANDING} dock requests outstanding",
                    host.short()
                );
            }
            work.queued.push_back(Job {
                id,
                request,
                deadline,
                answer,
            });
        }
        let _withdraw = Withdraw {
            link: link.clone(),
            id,
        };
        link.queued.notify_one();
        match tokio::time::timeout_at(deadline, answered).await {
            Ok(Ok(reply)) => Ok(reply),
            _ => Err(anyhow::anyhow!(
                "host {} did not answer over its uplink in time \
                 (if it had already taken the request, its outcome is unknown)",
                host.short()
            )),
        }
    }
}

/// Serve uplink polls on `bus` into `hub`. The poller is the verified envelope
/// signer, so a host can only take jobs addressed to its own fingerprint, and a
/// poll naming any other key gets no work and stages nothing. `promoted`
/// decides, per poll, whether the host is served or only staged (K8.5); it may
/// touch the disk, so it runs on a blocking thread.
pub(crate) fn serve(
    bus: &Bus,
    user_fp: Fingerprint,
    hub: Arc<UplinkHub>,
    promoted: impl Fn(&Hello) -> bool + Send + Sync + 'static,
) {
    let topic = Topic::new(user_fp, DOCK_UPLINK_TOPIC);
    let promoted = Arc::new(promoted);
    bus.handle_requests_with_context(topic, move |ctx: RequestContext, body| {
        let hub = hub.clone();
        let promoted = promoted.clone();
        async move {
            let poll: Poll = serde_json::from_slice(&body)?;
            let host = ctx.caller_agent_fp;
            if Fingerprint::of_bytes(&poll.host.pubkey) != host {
                tracing::warn!(host = %host.short(), "dock uplink: poll names another key; ignored");
                return Ok(serde_json::to_vec(&Work::default())?);
            }
            let hello = poll.host.clone();
            // Fail closed: a check that could not run promotes nobody.
            let admitted = tokio::task::spawn_blocking(move || promoted(&hello)).await;
            let work = if admitted.unwrap_or(false) {
                hub.poll(host, poll).await
            } else {
                hub.stage(host).await
            };
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

    impl Poll {
        /// A poll carrying `answer`. `UplinkHub::poll` does not read the
        /// hello; `serve` checks it before calling it.
        fn answering(answer: Option<(JobId, DockReply)>) -> Self {
            Self {
                host: Hello {
                    pubkey: [0; 32],
                    instance: "test".into(),
                },
                answer,
            }
        }
    }

    fn inject() -> DockRequest {
        DockRequest::Inject {
            conv: "c".into(),
            text: "run the lints".into(),
        }
    }

    impl UplinkHub {
        /// Record a poll from `host` without holding one open, as a host that
        /// keeps polling would between the steps of a test.
        fn polled_now(&self, host: Fingerprint) {
            *self.link(host).last_poll.lock().unwrap() = Instant::now();
        }

        /// Registrations still held for `host` (queued or dispatched).
        fn outstanding(&self, host: Fingerprint) -> usize {
            let link = self.link(host);
            let work = link.work.lock().unwrap();
            work.queued.len() + work.dispatched.len()
        }
    }

    /// Spawn `hub.request(host, req)` and let it run until it is waiting.
    async fn ask(
        hub: &Arc<UplinkHub>,
        host: Fingerprint,
        req: DockRequest,
    ) -> JoinHandle<anyhow::Result<DockReply>> {
        let task = tokio::spawn({
            let hub = hub.clone();
            async move { hub.request(host, req).await }
        });
        // Single-threaded runtime: one yield runs the request to its await.
        tokio::task::yield_now().await;
        task
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

    /// Liveness runs on Tokio's clock, so paused time can prove it: a host
    /// silent for longer than `POLL_TIMEOUT` takes no requests.
    #[tokio::test(start_paused = true)]
    async fn a_host_that_stopped_polling_takes_no_requests() {
        let hub = UplinkHub::default();
        drop(hub.link(fp(1)));
        tokio::time::advance(POLL_TIMEOUT + Duration::from_secs(1)).await;
        let err = hub
            .request(fp(1), DockRequest::ListSessions)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no open uplink"), "{err}");
    }

    /// A job reaches the polling host, its answer — carried on the host's
    /// next poll — completes the hub's request, and nothing stays registered.
    #[tokio::test(start_paused = true)]
    async fn a_job_rides_a_held_poll_and_its_answer_rides_the_next() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let asking = ask(&hub, fp(1), DockRequest::ListSessions).await;
        let Work {
            job: Some((id, DockRequest::ListSessions)),
            ..
        } = hub.poll(fp(1), Poll::answering(None)).await
        else {
            panic!("the poll carries the job");
        };
        let next = tokio::spawn({
            let hub = hub.clone();
            async move {
                let answer = Some((id, DockReply::Sessions(Vec::new())));
                hub.poll(fp(1), Poll::answering(answer)).await
            }
        });
        assert!(matches!(asking.await.unwrap(), Ok(DockReply::Sessions(s)) if s.is_empty()));
        assert!(next.await.unwrap().job.is_none(), "the hold lapses");
        assert_eq!(hub.outstanding(fp(1)), 0);
    }

    /// A host only ever receives jobs addressed to its own fingerprint (the
    /// poller is the verified envelope signer).
    #[tokio::test(start_paused = true)]
    async fn a_host_never_receives_another_hosts_job() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let asking = ask(&hub, fp(1), DockRequest::ListSessions).await;
        assert!(hub.poll(fp(2), Poll::answering(None)).await.job.is_none());
        assert!(asking.await.unwrap().is_err(), "host 1 never polled for it");
    }

    /// Before dispatch, a request that timed out is never sent: an `Inject`
    /// whose requester gave up must not run later.
    #[tokio::test(start_paused = true)]
    async fn an_expired_inject_is_never_dispatched() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let asking = ask(&hub, fp(1), inject()).await;
        assert!(asking.await.unwrap().is_err(), "times out before any poll");
        let work = hub.poll(fp(1), Poll::answering(None)).await;
        assert!(work.job.is_none(), "expired work delivered: {:?}", work.job);
    }

    /// The deadline is enforced at dispatch, not only by the requester noticing
    /// its own timeout: work past its deadline is not sent even while its
    /// requester has not yet been scheduled to wake and withdraw it.
    #[tokio::test(start_paused = true)]
    async fn work_past_its_deadline_is_not_dispatched_before_its_requester_wakes() {
        let hub = UplinkHub::default();
        drop(hub.link(fp(1)));
        let asking = hub.request(fp(1), inject());
        tokio::pin!(asking);
        // Poll the request once: it queues its job and waits.
        assert!(tokio::time::timeout(Duration::ZERO, &mut asking)
            .await
            .is_err());
        tokio::time::advance(ANSWER_TIMEOUT + Duration::from_secs(1)).await;
        hub.polled_now(fp(1));
        // `asking` is still alive and has not observed its deadline.
        assert_eq!(hub.outstanding(fp(1)), 1);
        let work = hub.poll(fp(1), Poll::answering(None)).await;
        assert!(work.job.is_none(), "expired work delivered: {:?}", work.job);
    }

    /// A requester dropped before dispatch leaves no registration and its
    /// work is never sent.
    #[tokio::test(start_paused = true)]
    async fn an_aborted_request_is_withdrawn_before_dispatch() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let asking = ask(&hub, fp(1), inject()).await;
        assert_eq!(hub.outstanding(fp(1)), 1);
        asking.abort();
        assert!(asking.await.unwrap_err().is_cancelled());
        assert_eq!(hub.outstanding(fp(1)), 0, "registration removed");
        let work = hub.poll(fp(1), Poll::answering(None)).await;
        assert!(
            work.job.is_none(),
            "withdrawn work delivered: {:?}",
            work.job
        );
    }

    /// Expired work neither blocks nor takes the place of a live request.
    #[tokio::test(start_paused = true)]
    async fn a_live_request_is_dispatched_past_expired_work() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let stale = ask(&hub, fp(1), inject()).await;
        assert!(stale.await.unwrap().is_err());
        hub.polled_now(fp(1));
        let _live = ask(&hub, fp(1), DockRequest::ListSessions).await;
        let work = hub.poll(fp(1), Poll::answering(None)).await;
        assert!(
            matches!(work.job, Some((_, DockRequest::ListSessions))),
            "poll carried {:?} instead of the live request",
            work.job
        );
    }

    /// A late answer — for a request that already timed out — completes
    /// nothing, and a request issued after it is unaffected.
    #[tokio::test(start_paused = true)]
    async fn a_late_answer_completes_no_other_request() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let first = ask(&hub, fp(1), DockRequest::ListSessions).await;
        let Work {
            job: Some((late, _)),
            ..
        } = hub.poll(fp(1), Poll::answering(None)).await
        else {
            panic!("the first request is dispatched");
        };
        assert!(first.await.unwrap().is_err(), "it times out unanswered");
        hub.polled_now(fp(1));
        let mut second = ask(&hub, fp(1), DockRequest::ListSessions).await;
        let _held = tokio::spawn({
            let hub = hub.clone();
            async move {
                let answer = Some((late, DockReply::Injected));
                hub.poll(fp(1), Poll::answering(answer)).await
            }
        });
        let settled = tokio::time::timeout(Duration::from_secs(1), &mut second).await;
        assert!(settled.is_err(), "the late answer resolved another request");
    }

    /// Restart safety: an answer a host retained from an old hub instance can
    /// never resolve a request on the new one, even after the new hub's first
    /// response to the host is lost and the host retries the old answer.
    #[tokio::test(start_paused = true)]
    async fn an_answer_from_before_a_hub_restart_resolves_nothing() {
        let old = Arc::new(UplinkHub::default());
        drop(old.link(fp(1)));
        let _pending = ask(&old, fp(1), DockRequest::ListSessions).await;
        let Work {
            job: Some((old_id, _)),
            ..
        } = old.poll(fp(1), Poll::answering(None)).await
        else {
            panic!("the old hub dispatches");
        };
        drop(old); // the hub restarts

        let new = Arc::new(UplinkHub::default());
        drop(new.link(fp(1)));
        let early = Some((old_id, DockReply::Injected));
        assert!(new.poll(fp(1), Poll::answering(early)).await.job.is_none());
        let mut asking = ask(&new, fp(1), DockRequest::ListSessions).await;
        let _lost = new.poll(fp(1), Poll::answering(None)).await; // response lost
        let _retry = tokio::spawn({
            let new = new.clone();
            async move {
                let retained = Some((old_id, DockReply::Sessions(Vec::new())));
                new.poll(fp(1), Poll::answering(retained)).await
            }
        });
        let settled = tokio::time::timeout(Duration::from_secs(1), &mut asking).await;
        assert!(
            settled.is_err(),
            "resolved by the old hub's answer: {settled:?}"
        );
    }

    /// An answer is matched only within the answering host's own work, and
    /// only once.
    #[tokio::test(start_paused = true)]
    async fn duplicate_and_wrong_host_answers_are_ignored() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        drop(hub.link(fp(2)));
        let mut asking = ask(&hub, fp(1), DockRequest::ListSessions).await;
        let Work {
            job: Some((id, _)), ..
        } = hub.poll(fp(1), Poll::answering(None)).await
        else {
            panic!("dispatched to host 1");
        };
        let wrong = Some((id, DockReply::Injected));
        let _other = tokio::spawn({
            let hub = hub.clone();
            async move { hub.poll(fp(2), Poll::answering(wrong)).await }
        });
        let settled = tokio::time::timeout(Duration::from_secs(1), &mut asking).await;
        assert!(settled.is_err(), "host 2 answered host 1's job");

        let right = Some((id, DockReply::Sessions(Vec::new())));
        let _answer = tokio::spawn({
            let hub = hub.clone();
            async move { hub.poll(fp(1), Poll::answering(right)).await }
        });
        assert!(matches!(asking.await.unwrap(), Ok(DockReply::Sessions(_))));
        let again = ask(&hub, fp(1), DockRequest::ListSessions).await;
        let dup = Some((id, DockReply::Injected));
        let _dup = tokio::spawn({
            let hub = hub.clone();
            async move { hub.poll(fp(1), Poll::answering(dup)).await }
        });
        let mut again = again;
        let settled = tokio::time::timeout(Duration::from_secs(1), &mut again).await;
        assert!(
            settled.is_err(),
            "a duplicate answer resolved a new request"
        );
    }

    /// Retained work is bounded: a request past the limit fails at once, and
    /// withdrawn or expired work does not count against it.
    #[tokio::test(start_paused = true)]
    async fn outstanding_work_per_host_is_bounded() {
        let hub = Arc::new(UplinkHub::default());
        drop(hub.link(fp(1)));
        let mut asks = Vec::new();
        for _ in 0..MAX_OUTSTANDING {
            asks.push(ask(&hub, fp(1), DockRequest::ListSessions).await);
        }
        let err = hub
            .request(fp(1), DockRequest::ListSessions)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outstanding"), "{err}");
        for task in &asks {
            task.abort();
        }
        for task in asks {
            let _ = task.await;
        }
        assert_eq!(hub.outstanding(fp(1)), 0);
        let _fits = ask(&hub, fp(1), DockRequest::ListSessions).await;
        assert_eq!(hub.outstanding(fp(1)), 1, "room again once withdrawn");
    }

    #[tokio::test(start_paused = true)]
    async fn a_staged_poll_drops_the_hosts_link_and_carries_no_job() {
        let hub = UplinkHub::default();
        hub.poll(fp(1), Poll::answering(None)).await;
        assert!(
            hub.live_link(fp(1)).is_some(),
            "a promoted poll opens a link"
        );

        let work = hub.stage(fp(1)).await;
        assert!(work.staged && work.job.is_none());
        assert!(hub.live_link(fp(1)).is_none(), "staging drops the link");
        let unreached = hub.request(fp(1), DockRequest::ListSessions).await;
        assert!(unreached.is_err(), "the hub cannot reach a staged host");
    }

    // ── Shutdown, over real loopback QUIC (the dock-security lanes run these) ──

    use crate::dock::{dock_agent, DockClient, DockRole};
    use std::net::{Ipv4Addr, UdpSocket};

    /// The uplink's UDP port can be bound again within `within`: nothing
    /// holds it any more. Polled, since the socket is released as the
    /// endpoint's last handle drops.
    async fn port_released(port: u16, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < deadline {
            if UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// How long a closed uplink's port may stay bound: `Bus::close` closes the
    /// endpoint (agent-mesh#98), so release is prompt even with a poll's reply
    /// window (`POLL_TIMEOUT`) still open.
    const RELEASE_BOUND: Duration = Duration::from_secs(5);

    async fn hub_for(user: &UserKey) -> (DockClient, PeerEndpoint) {
        let agent = dock_agent(user, DockRole::Hub, "hub");
        let pubkey = agent.public_bytes();
        let hub = DockClient::bind(user, agent, 0).await.unwrap();
        hub.serve_all_uplinks();
        let addr = (Ipv4Addr::LOCALHOST, hub.local_port()).into();
        (hub, PeerEndpoint::new(pubkey, addr))
    }

    /// Start an uplink as host `instance`; returns it with its fingerprint.
    async fn uplink_to(
        user: &UserKey,
        instance: &str,
        hub: PeerEndpoint,
    ) -> (DockUplink, Fingerprint) {
        let host_fp = dock_agent(user, DockRole::Host, instance).fingerprint();
        let dir = std::env::temp_dir();
        let uplink = DockUplink::start(user, instance, dir, hub).await.unwrap();
        (uplink, host_fp)
    }

    /// Close `uplink` within 5s and check its runner finished.
    async fn close_within_5s(uplink: DockUplink, what: &str) {
        let state = uplink.state.clone();
        tokio::time::timeout(Duration::from_secs(5), uplink.close())
            .await
            .unwrap_or_else(|_| panic!("{what}: close returns within 5s"));
        assert_eq!(
            *state.borrow(),
            UplinkState::Closed,
            "{what}: runner finished"
        );
    }

    /// Close while the hub is holding the uplink's poll: `close` returns
    /// promptly with the runner finished and the bus closed, and the port is
    /// released within [`RELEASE_BOUND`].
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live transport — nightly/full mesh-integration tier only"]
    async fn dock_uplink_closes_during_an_outstanding_poll() {
        let user = UserKey::generate();
        let (hub, hub_ep) = hub_for(&user).await;
        let (uplink, host_fp) = uplink_to(&user, "laptop", hub_ep).await;
        tokio::time::timeout(
            Duration::from_secs(5),
            hub.uplinks().wait_for_uplink(host_fp),
        )
        .await
        .expect("the hub is holding a poll");
        assert_eq!(uplink.state(), UplinkState::Polling);
        let port = uplink.local_port();
        close_within_5s(uplink, "held poll").await;
        assert!(port_released(port, RELEASE_BOUND).await, "port still bound");
        hub.close().await.unwrap();
    }

    /// Close while backing off after failed polls (the "hub" refuses every
    /// connection): `close` returns promptly and the port is released within
    /// [`RELEASE_BOUND`].
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live transport — nightly/full mesh-integration tier only"]
    async fn dock_uplink_closes_during_backoff() {
        let user = UserKey::generate();
        let refusing = Bus::bind_outbound_only(&user, dock_agent(&user, DockRole::Hub, "x"))
            .await
            .unwrap();
        let hub_ep = PeerEndpoint::new(
            dock_agent(&user, DockRole::Hub, "x").public_bytes(),
            (Ipv4Addr::LOCALHOST, refusing.local_port()).into(),
        );
        let (mut uplink, _) = uplink_to(&user, "laptop", hub_ep).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            uplink.reached(UplinkState::Backoff),
        )
        .await
        .expect("a refused poll puts the uplink in backoff");
        let port = uplink.local_port();
        close_within_5s(uplink, "backoff").await;
        assert!(port_released(port, RELEASE_BOUND).await, "port still bound");
        refusing.close().await.unwrap();
    }

    /// Repeated start/close cycles against one hub: each cycle's host (its own
    /// instance, so its own fingerprint) polls, then closes with its runner
    /// finished; every port is released within [`RELEASE_BOUND`].
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live transport — nightly/full mesh-integration tier only"]
    async fn dock_uplink_survives_repeated_start_close_cycles() {
        let user = UserKey::generate();
        let (hub, hub_ep) = hub_for(&user).await;
        let mut ports = Vec::new();
        for cycle in 0..5 {
            let (uplink, host_fp) = uplink_to(&user, &format!("laptop-{cycle}"), hub_ep).await;
            tokio::time::timeout(
                Duration::from_secs(5),
                hub.uplinks().wait_for_uplink(host_fp),
            )
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle}: the uplink polls"));
            ports.push(uplink.local_port());
            close_within_5s(uplink, &format!("cycle {cycle}")).await;
        }
        for port in ports {
            assert!(
                port_released(port, RELEASE_BOUND).await,
                "port {port} still bound"
            );
        }
        hub.close().await.unwrap();
    }
}
