//! Invocation-owned broker services and the worker's static-filter adapter.
//!
//! All messages use the separate authenticated channel. No shell output is
//! parsed as authority. The host retains every service/resource through worker
//! reaping; cancellation closes sockets and joins service threads.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_bridle_core::{ToolContext, ToolError, ToolResult};
use brush_core::extensions::ShellExtensions;
use brush_core::filter::{DelegatedFd, ExternalCmdParams, ExternalCommand};
use content_addressable::{canonical, ContentId, MerkleNode};
use serde::{Deserialize, Serialize};

use crate::broker_transport::{self, BrokerClient, RpcReply};
use crate::command_broker::{
    BrokerCommand, BrokerControl, BrokerProcess, BrokerSession, CommandBroker,
};

const MAX_SESSIONS: usize = 64;
const MAX_REQUESTS: usize = 32;

#[derive(Serialize, Deserialize)]
enum Request {
    Prepare(BrokerCommand),
    Spawn { invocation: ContentId, pid: u32 },
}

#[derive(Clone, Serialize, Deserialize)]
struct Preparation {
    // Freshness locator; the actual identity covers the entire preparation.
    #[serde(with = "serde_bytes")]
    challenge: Vec<u8>,
    args: Option<Vec<OsString>>,
    env: Vec<(OsString, OsString)>,
    target_fd: i32,
}

impl Preparation {
    fn id(&self) -> ToolResult<ContentId> {
        MerkleNode::new(self.clone(), BTreeSet::new())
            .id()
            .map_err(error)
    }
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Prepared(Option<(ContentId, Preparation)>),
    Spawned,
}

fn error(error: impl std::fmt::Display) -> ToolError {
    ToolError::denied(error.to_string())
}

fn encode(value: &impl Serialize) -> ToolResult<Vec<u8>> {
    canonical::to_canonical_dagcbor(value).map_err(error)
}

fn decode<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8]) -> ToolResult<T> {
    canonical::from_canonical_dagcbor_checked(bytes).map_err(error)
}

struct Session {
    policy: Arc<dyn BrokerSession>,
    program: std::path::PathBuf,
    expected_image: std::path::PathBuf,
    registered: Mutex<Option<BrokerProcess>>,
    ready: Condvar,
}

struct State {
    broker: Arc<dyn CommandBroker>,
    context: ToolContext,
    control: BrokerControl,
    worker_pid: u32,
    stopping: AtomicBool,
    sessions: Mutex<HashMap<ContentId, Arc<Session>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    sockets: Mutex<HashMap<u64, UnixStream>>,
    next_socket: AtomicU64,
    active: AtomicUsize,
}

struct SocketLease {
    state: Arc<State>,
    key: u64,
}
impl Drop for SocketLease {
    fn drop(&mut self) {
        self.state.sockets.lock().unwrap().remove(&self.key);
    }
}

impl State {
    fn retain_thread(&self, thread: JoinHandle<()>) {
        let finished = {
            let mut threads = self.threads.lock().unwrap();
            let mut finished = Vec::new();
            let mut index = 0;
            while index < threads.len() {
                if threads[index].is_finished() {
                    finished.push(threads.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            threads.push(thread);
            finished
        };
        for thread in finished {
            let _ = thread.join();
        }
    }

    fn track(self: &Arc<Self>, socket: &UnixStream) -> ToolResult<SocketLease> {
        let key = self.next_socket.fetch_add(1, Ordering::Relaxed);
        let mut sockets = self.sockets.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            return Err(error("broker stopped"));
        }
        sockets.insert(key, socket.try_clone()?);
        Ok(SocketLease {
            state: self.clone(),
            key,
        })
    }

    fn check(&self) -> ToolResult<()> {
        self.control.check()?;
        if self.stopping.load(Ordering::Acquire) {
            return Err(error("broker stopped"));
        }
        Ok(())
    }

    fn listen(
        self: &Arc<Self>,
        endpoint: UnixStream,
        session: Option<Arc<Session>>,
    ) -> ToolResult<()> {
        endpoint.set_read_timeout(Some(Duration::from_millis(50)))?;
        let lease = self.track(&endpoint)?;
        let state = self.clone();
        let thread = std::thread::spawn(move || {
            let _lease = lease;
            while state.check().is_ok() {
                let stream = match agent_bridle_core::receive_control_endpoint(&endpoint) {
                    Ok(stream) => stream,
                    Err(ToolError::Exec(ref err))
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(_) => break,
                };
                if state.active.fetch_add(1, Ordering::AcqRel) >= MAX_REQUESTS {
                    state.active.fetch_sub(1, Ordering::AcqRel);
                    drop(stream);
                    continue;
                }
                let request_lease = match state.track(&stream) {
                    Ok(lease) => lease,
                    Err(_) => {
                        state.active.fetch_sub(1, Ordering::AcqRel);
                        break;
                    }
                };
                let request_state = state.clone();
                let request_session = session.clone();
                let request_thread = std::thread::spawn(move || {
                    let _lease = request_lease;
                    let expected = request_session
                        .is_none()
                        .then_some(request_state.worker_pid);
                    let _ = broker_transport::serve_request(
                        stream,
                        &request_state.control,
                        expected,
                        |peer, payload| {
                            request_state.check()?;
                            if let Some(session) = request_session {
                                let mut registered = session.registered.lock().unwrap();
                                while registered.is_none() {
                                    request_state.check()?;
                                    registered = session
                                        .ready
                                        .wait_timeout(registered, Duration::from_millis(10))
                                        .unwrap()
                                        .0;
                                }
                                let process = registered.as_ref().unwrap();
                                if peer.parent_process_id != process.process_id
                                    || peer.parent_birth != process.birth
                                    || peer.parent_executable != session.expected_image
                                {
                                    return Err(error(
                                        "helper does not belong to the registered native process",
                                    ));
                                }
                                drop(registered);
                                let payload = session.policy.request(
                                    peer,
                                    payload,
                                    &request_state.control,
                                )?;
                                Ok(RpcReply {
                                    payload,
                                    endpoint: None,
                                })
                            } else {
                                request_state.worker_request(payload)
                            }
                        },
                    );
                    request_state.active.fetch_sub(1, Ordering::AcqRel);
                });
                state.retain_thread(request_thread);
            }
        });
        self.retain_thread(thread);
        Ok(())
    }

    fn worker_request(self: &Arc<Self>, bytes: &[u8]) -> ToolResult<RpcReply> {
        match decode(bytes)? {
            Request::Prepare(command) => {
                let prepared = self
                    .broker
                    .prepare(&command, &self.context, &self.control)?;
                let Some(prepared) = prepared else {
                    return Ok(RpcReply {
                        payload: encode(&Reply::Prepared(None))?,
                        endpoint: None,
                    });
                };
                if prepared.target_fd < 3 {
                    return Err(error("broker endpoint target must be above stdio"));
                }
                let program = std::fs::canonicalize(&command.program)?;
                let expected_image = match prepared.expected_process_image {
                    Some(image) => std::fs::canonicalize(image)?,
                    None => program.clone(),
                };
                let mut challenge = vec![0; 32];
                getrandom::getrandom(&mut challenge).map_err(error)?;
                let wire = Preparation {
                    challenge,
                    args: prepared.args,
                    env: prepared.env,
                    target_fd: prepared.target_fd,
                };
                let id = wire.id()?;
                let session = Arc::new(Session {
                    policy: prepared.session,
                    program,
                    expected_image,
                    registered: Mutex::new(None),
                    ready: Condvar::new(),
                });
                {
                    let mut sessions = self.sessions.lock().unwrap();
                    if sessions.len() >= MAX_SESSIONS {
                        return Err(error("broker invocation session limit reached"));
                    }
                    if sessions.insert(id, session.clone()).is_some() {
                        return Err(error("duplicate broker invocation identity"));
                    }
                }
                let (host, worker) = UnixStream::pair()?;
                self.listen(host, Some(session))?;
                Ok(RpcReply {
                    payload: encode(&Reply::Prepared(Some((id, wire))))?,
                    endpoint: Some(worker),
                })
            }
            Request::Spawn { invocation, pid } => {
                let session = self
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&invocation)
                    .cloned()
                    .ok_or_else(|| error("unknown broker invocation"))?;
                let mut registered = session.registered.lock().unwrap();
                if registered.is_some() {
                    return Err(error("broker native process already registered"));
                }
                let (parent, birth) = broker_transport::process_parent_and_birth(pid)?;
                let executable = broker_transport::executable_path(pid)?;
                if parent != self.worker_pid
                    || (executable != session.program && executable != session.expected_image)
                {
                    return Err(error(
                        "native spawn does not match the admitted worker command",
                    ));
                }
                let process = BrokerProcess {
                    process_id: pid,
                    executable,
                    birth,
                };
                session.policy.on_spawn(&process, &self.control)?;
                self.check()?;
                *registered = Some(process);
                session.ready.notify_all();
                Ok(RpcReply {
                    payload: encode(&Reply::Spawned)?,
                    endpoint: None,
                })
            }
        }
    }
}

/// Dropped only after the owning worker has stopped and been reaped.
pub(crate) struct BrokerRuntime {
    state: Arc<State>,
}

impl BrokerRuntime {
    pub(crate) fn start(
        broker: Arc<dyn CommandBroker>,
        context: ToolContext,
        worker_pid: u32,
        cancelled: Arc<AtomicBool>,
        deadline: Instant,
    ) -> ToolResult<(Self, UnixStream)> {
        let state = Arc::new(State {
            broker,
            context,
            control: BrokerControl {
                cancelled,
                deadline,
            },
            worker_pid,
            stopping: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            threads: Mutex::new(Vec::new()),
            sockets: Mutex::new(HashMap::new()),
            next_socket: AtomicU64::new(0),
            active: AtomicUsize::new(0),
        });
        let (host, worker) = UnixStream::pair()?;
        state.listen(host, None)?;
        Ok((Self { state }, worker))
    }
}

impl Drop for BrokerRuntime {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        for socket in self.state.sockets.lock().unwrap().values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        loop {
            let threads = std::mem::take(&mut *self.state.threads.lock().unwrap());
            if threads.is_empty() {
                break;
            }
            for thread in threads {
                let _ = thread.join();
            }
        }
    }
}

/// Worker-side state is unforgeable from shell variables or output. The source
/// FD object itself correlates the prepared launch, including concurrent jobs.
pub(crate) struct WorkerBroker {
    client: BrokerClient,
    prepared: Mutex<HashMap<i32, (ContentId, DelegatedFd)>>,
}

impl std::fmt::Debug for WorkerBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerBroker").finish_non_exhaustive()
    }
}

impl WorkerBroker {
    pub(crate) fn new(endpoint: UnixStream) -> Self {
        Self {
            client: BrokerClient::new(endpoint),
            prepared: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn prepare<SE: ShellExtensions>(
        &self,
        params: &mut ExternalCmdParams<'_, SE>,
    ) -> ToolResult<()> {
        let cwd = params
            .command
            .current_dir()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| error("broker requires explicit absolute cwd"))?
            .to_path_buf();
        let program = std::path::PathBuf::from(params.command.program());
        let program = if program.is_absolute() {
            program
        } else {
            cwd.join(program)
        };
        let original = BrokerCommand {
            program,
            original_command: params.original_command().to_owned(),
            argv0: params.command.argv0().to_owned(),
            args: params.command.args().to_vec(),
            env: params.command.envs().to_vec(),
            cwd,
        };
        let reply = self.client.request_inner(
            &encode(&Request::Prepare(original))?,
            Duration::from_secs(30),
        )?;
        let Reply::Prepared(prepared) = decode(&reply.payload)? else {
            return Err(error("unexpected broker prepare reply"));
        };
        let Some((id, prepared)) = prepared else {
            if reply.endpoint.is_some() {
                return Err(error("unchanged command received an unexpected endpoint"));
            }
            return Ok(());
        };
        if prepared.id()? != id {
            return Err(error("broker preparation content identity mismatch"));
        }
        let endpoint = reply
            .endpoint
            .ok_or_else(|| error("broker preparation omitted its endpoint"))?;
        let fd = DelegatedFd::new(prepared.target_fd, OwnedFd::from(endpoint)).map_err(error)?;
        // Rebuild only when native argv was prepared. Preserve every original
        // property, including prior explicit delegations and env-clear intent.
        if let Some(args) = prepared.args {
            let previous = &params.command;
            let mut command = ExternalCommand::new(previous.program());
            command.set_argv0(previous.argv0()).args_extend(args);
            if previous.env_clear() {
                command.clear_env();
            }
            for (key, value) in previous.envs() {
                command.env(key, value);
            }
            if let Some(cwd) = previous.current_dir() {
                command.set_current_dir(cwd);
            }
            for delegation in previous.delegated_fds() {
                command.delegate_fd(delegation.clone()).map_err(error)?;
            }
            params.command = command;
        }
        for (key, value) in prepared.env {
            params.command.env(key, value);
        }
        params.command.delegate_fd(fd.clone()).map_err(error)?;
        let source = fd.source().as_raw_fd();
        self.prepared.lock().unwrap().insert(source, (id, fd));
        Ok(())
    }

    pub(crate) fn check_delegations(&self, command: &ExternalCommand) -> ToolResult<()> {
        let prepared = self.prepared.lock().unwrap();
        for fd in command.delegated_fds() {
            if !prepared
                .get(&fd.source().as_raw_fd())
                .is_some_and(|(_, known)| known.target() == fd.target())
            {
                return Err(error("command carried an undeclared private descriptor"));
            }
        }
        Ok(())
    }

    pub(crate) fn spawned(&self, command: &ExternalCommand, pid: Option<u32>) -> ToolResult<()> {
        let prepared = self.prepared.lock().unwrap();
        for fd in command.delegated_fds() {
            let (id, _) = prepared
                .get(&fd.source().as_raw_fd())
                .ok_or_else(|| error("unknown broker descriptor at spawn"))?;
            let reply = self.client.request_inner(
                &encode(&Request::Spawn {
                    invocation: *id,
                    pid: pid.ok_or_else(|| {
                        error("platform cannot identify the spawned broker command")
                    })?,
                })?,
                Duration::from_secs(30),
            )?;
            if !matches!(decode::<Reply>(&reply.payload)?, Reply::Spawned)
                || reply.endpoint.is_some()
            {
                return Err(error("unexpected broker spawn reply"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridle_core::{Caveats, Gate, Tool};
    use std::io::Read;

    struct ContextTool;
    #[async_trait::async_trait]
    impl Tool for ContextTool {
        fn name(&self) -> &str {
            "broker-context-fixture"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn invoke(
            &self,
            _: serde_json::Value,
            _: &ToolContext,
        ) -> ToolResult<serde_json::Value> {
            unreachable!()
        }
    }

    struct InspectBroker {
        expected: Caveats,
        seen: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for InspectBroker {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    impl CommandBroker for InspectBroker {
        fn prepare(
            &self,
            _: &BrokerCommand,
            context: &ToolContext,
            _: &BrokerControl,
        ) -> ToolResult<Option<crate::PreparedBrokerCommand>> {
            assert_eq!(context.caveats(), &self.expected);
            self.seen.store(true, Ordering::Release);
            Ok(None)
        }
    }

    fn fixture() -> (
        Arc<InspectBroker>,
        ToolContext,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let grant = Caveats {
            fs_write: agent_bridle_core::Scope::Only(BTreeSet::new()),
            ..Caveats::top()
        };
        let context = Gate::new(0).authorize(&ContextTool, &grant).unwrap();
        let seen = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        (
            Arc::new(InspectBroker {
                expected: context.caveats().clone(),
                seen: seen.clone(),
                dropped: dropped.clone(),
            }),
            context,
            seen,
            dropped,
        )
    }

    #[test]
    fn broker_preserves_context_and_releases_owner_after_services_join() {
        let (broker, context, seen, dropped) = fixture();
        let (runtime, endpoint) = BrokerRuntime::start(
            broker,
            context,
            std::process::id(),
            Arc::new(AtomicBool::new(false)),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        let command = BrokerCommand {
            program: std::env::current_exe().unwrap(),
            original_command: "fixture".into(),
            argv0: "fixture".into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: std::env::current_dir().unwrap(),
        };
        let reply = BrokerClient::new(endpoint)
            .request_inner(
                &encode(&Request::Prepare(command)).unwrap(),
                Duration::from_secs(5),
            )
            .unwrap();
        assert!(matches!(
            decode::<Reply>(&reply.payload).unwrap(),
            Reply::Prepared(None)
        ));
        assert!(seen.load(Ordering::Acquire));
        assert!(!dropped.load(Ordering::Acquire));
        drop(runtime);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_closes_pending_handshake_before_releasing_resources() {
        let (broker, context, seen, dropped) = fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (runtime, endpoint) = BrokerRuntime::start(
            broker,
            context,
            std::process::id(),
            cancelled.clone(),
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        let (mut stalled, remote) = UnixStream::pair().unwrap();
        stalled
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        agent_bridle_core::send_control_endpoint(&endpoint, &remote).unwrap();
        drop(remote);
        // The server has accepted the socket and is waiting for client proof.
        let mut hello = [0; 44];
        stalled.read_exact(&mut hello).unwrap();
        assert!(!dropped.load(Ordering::Acquire));
        cancelled.store(true, Ordering::Release);
        let start = Instant::now();
        drop(runtime);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!seen.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
        let mut byte = [0];
        assert_eq!(stalled.read(&mut byte).unwrap(), 0);
    }
}
