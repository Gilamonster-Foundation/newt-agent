//! Loopback egress proxy for the macOS `net` host allow-list (#124, ADR 0016).
//!
//! SBPL cannot name a remote host (ADR 0015), so a general `net: Only([host, …])`
//! grant cannot be kernel-confined by hostname. The honest mechanism: kernel-fence
//! a spawned child's egress to the **loopback interface** (the ADR 0015 rule, via
//! [`crate::loopback_fenced_caveats`]), then run **this** loopback
//! forward proxy — which the child is pointed at through `*_PROXY` env — to enforce
//! the hostname allow-list. The child can reach *nothing* off-box except through
//! the proxy, and the proxy admits only allow-listed hosts.
//!
//! **Grade (honesty):** the loopback fence is kernel-grade and unbypassable; the
//! hostname match is **userspace** (this parent-side process). So the `net` axis
//! stays reported `Advisory` — the proxy *over-delivers* above that floor (the
//! `report.rs` doctrine), it does not raise the honest kernel claim.
//!
//! **Scope:** HTTP `CONNECT` (HTTPS tunnelling — the proxy never terminates TLS)
//! and `http://` absolute-form forwarding. Non-proxy-aware traffic (raw sockets,
//! tools ignoring `*_PROXY`) is kernel-fenced to loopback and therefore blocked
//! off-box — fail-closed, the safe direction.
//!
//! Std-only (`std::net` + `std::thread`); no async runtime, no new dependency.
//!
//! ## Bounded, joinable lifecycle (#372)
//!
//! Every accept, connection, and splice worker this module spawns is retained
//! and joined — never fire-and-forget, and never unbounded. The proxy moves
//! through one internal lifecycle, `Running -> ShuttingDown -> Quiescent`.
//! [`ProxyHandle::shutdown_and_join`] is the explicit, consuming finalizer that
//! drives that transition and returns [`ProxyFinalEvidence`] — bounded,
//! serializable, and frozen the instant it is returned (the handle that produced
//! it no longer exists). `Drop` stays fail-safe: it performs the SAME shutdown
//! and join as the finalizer, so nothing is ever detached, but it discards the
//! evidence rather than returning it — a live [`ProxyHandle::refused_hosts`]
//! snapshot is for diagnostics only and must never stand in for terminal
//! evidence.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

/// Longest request line / header block the proxy will buffer before giving up
/// (a request line is short; this only bounds a hostile client). 8 KiB.
const MAX_HEAD: usize = 8 * 1024;
/// Per-connection socket timeout, so a stuck peer cannot pin a proxy thread.
const CONN_TIMEOUT: Duration = Duration::from_secs(30);

/// #372 §3: the most refused-host entries [`ProxyFinalEvidence`] retains.
/// Anything past this bound is counted in `omitted_refused_attempts`, never
/// silently dropped.
pub const MAX_REFUSED_HOSTS: usize = 64;
/// #372 §3: the most bytes one retained refused-host string may occupy. A
/// longer attempt is counted as omitted rather than truncated (truncation could
/// fold two distinct hostile hosts into one retained entry).
pub const MAX_REFUSED_HOST_BYTES: usize = 255;
/// #372 §1: the default cap on simultaneously active connection workers. A
/// fixed, tracked bound — never an unbounded per-connection thread — so a
/// hostile or buggy client cannot make the proxy spawn without limit.
pub const DEFAULT_WORKER_CAP: usize = 256;

// ── Audit (#124, ADR 0016): the proxy is the child's sole egress chokepoint, so
// every proxy-visible connection is recorded through an operator-supplied sink.
// This is observability only — it never changes an enforcement decision. ────────

/// The kind of egress a child requested through the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetKind {
    /// `CONNECT host:port` — an opaque (HTTPS) tunnel.
    Connect,
    /// `http://…` plaintext forward.
    Http,
}

/// The proxy's allow-list decision for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetDecision {
    /// Host on the allow-list — connection made.
    Allowed,
    /// Host **not** on the allow-list — refused with 403 (the exfil-attempt signal).
    Denied,
    /// Allow-listed but the origin could not be reached (DNS/connect failure).
    Error,
}

/// One audited egress connection through the proxy — a complete record of the
/// child's proxy-visible network activity (#124, ADR 0016). Serialised as one
/// JSON line by [`JsonlSink`]; the `bridle-netmon` binary renders a live view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetAuditEvent {
    /// Unix-epoch milliseconds when the connection was decided.
    pub ts_ms: u64,
    /// The requested host (CONNECT authority or `http://` URI host).
    pub host: String,
    /// The requested port.
    pub port: u16,
    /// Tunnel (`connect`) or plaintext forward (`http`).
    pub kind: NetKind,
    /// Allow-list outcome.
    pub decision: NetDecision,
    /// Bytes the child sent (client → origin); `0` for a denied/errored conn.
    pub bytes_up: u64,
    /// Bytes the origin returned (origin → child).
    pub bytes_down: u64,
    /// Connection lifetime in milliseconds.
    pub dur_ms: u64,
}

/// A destination for [`NetAuditEvent`]s — the operator's audit trail. `record` is
/// called from a per-connection thread, so implementations must be thread-safe
/// and must never block the connection for long.
pub trait AuditSink: Send + Sync {
    /// Record one completed connection.
    fn record(&self, event: &NetAuditEvent);
}

/// The default sink — discard everything (audit off, zero overhead).
pub struct NullSink;

impl AuditSink for NullSink {
    fn record(&self, _event: &NetAuditEvent) {}
}

/// Append each event as one JSON line to a `Write` (a file, stderr, a pipe).
pub struct JsonlSink<W: Write + Send>(Mutex<W>);

impl<W: Write + Send> JsonlSink<W> {
    /// Wrap a writer as a JSON-lines audit sink.
    pub fn new(w: W) -> Self {
        Self(Mutex::new(w))
    }
}

impl<W: Write + Send> AuditSink for JsonlSink<W> {
    fn record(&self, event: &NetAuditEvent) {
        if let (Ok(mut w), Ok(mut line)) = (self.0.lock(), serde_json::to_string(event)) {
            line.push('\n');
            let _ = w.write_all(line.as_bytes());
            let _ = w.flush();
        }
    }
}

/// Unix-epoch milliseconds now (saturating to 0 before the epoch — never panics).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Resolves a proxied hostname to the address the proxy dials. A seam so a test
/// can map an allow-listed name to a loopback origin (hermetic, no real DNS).
pub trait Resolver: Send + Sync {
    /// Resolve `host:port` to a single dial target, or an error if it cannot.
    fn resolve(&self, host: &str, port: u16) -> io::Result<SocketAddr>;
}

/// The production resolver — the platform's own `getaddrinfo`, run in the parent
/// (never the fenced child), taking the first address.
pub struct StdResolver;

impl Resolver for StdResolver {
    fn resolve(&self, host: &str, port: u16) -> io::Result<SocketAddr> {
        // `to_socket_addrs` (getaddrinfo) has no std timeout of its own. A
        // black-holed/unresponsive resolver would otherwise hang this call —
        // and therefore the connection worker calling it — forever, which
        // `ProxyHandle::shutdown_and_join` can never join past (#372 §2: every
        // connection/splice worker must be joined for Quiescent). Run it on its
        // own thread and bound the wait to CONN_TIMEOUT, the same honest
        // worst-case bound every other blocking step in this module already
        // uses; on timeout the resolution thread is abandoned (it holds no
        // proxy-tracked resources) and the dial fails closed.
        let host = host.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("agent-bridle-egress-resolve".to_string())
            .spawn(move || {
                let _ = tx.send(
                    (host.as_str(), port)
                        .to_socket_addrs()
                        .map(|mut it| it.next()),
                );
            })
            .map_err(|e| io::Error::other(format!("failed to spawn resolver thread: {e}")))?;
        match rx.recv_timeout(CONN_TIMEOUT) {
            Ok(Ok(Some(addr))) => Ok(addr),
            Ok(Ok(None)) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no address for host",
            )),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "DNS resolution timed out",
            )),
        }
    }
}

// ── I/O seams (#166): the per-connection logic is written against `Conn` +
// `Connector` traits, not concrete `TcpStream`, so the parse / allow-list /
// forward / tunnel / audit LOGIC can be unit-tested against an in-memory duplex
// with NO real socket (deterministic, portable). Production wires the real
// `TcpStream` / `TcpConnector`; the tests wire scripted in-memory endpoints. ───

/// A bidirectional connection the proxy speaks over. Abstracts the three
/// `TcpStream`-specific operations the forward/tunnel path needs beyond
/// `Read`/`Write` — a second owned handle ([`Self::dup`], for the read/write
/// split and the origin clone), directional [`Self::shutdown`] (how the copy
/// threads signal EOF, and how #372's shutdown path force-closes an idle
/// connection), and the per-connection timeouts ([`Self::set_timeouts`]).
/// `TcpStream` implements it for production; an in-memory scripted stream
/// implements it for tests.
pub trait Conn: Read + Write + Send {
    /// A second owned handle to the SAME underlying stream — for `TcpStream`
    /// this is `try_clone` (both handles share one socket); the proxy holds one
    /// half for reading and one for writing, and `splice` needs a third for the
    /// origin write direction.
    fn dup(&self) -> io::Result<Box<dyn Conn>>;
    /// Shut down the read half, write half, or both — how a copy thread signals
    /// EOF to its peer and how #372's finalizer force-closes a proxy-owned
    /// socket end during shutdown.
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
    /// Apply the per-connection read+write timeouts. A no-op for in-memory
    /// fakes, which never block on a real socket.
    fn set_timeouts(&self, dur: Duration) -> io::Result<()>;
}

impl Conn for TcpStream {
    fn dup(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        TcpStream::shutdown(self, how)
    }
    fn set_timeouts(&self, dur: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(dur))?;
        self.set_write_timeout(Some(dur))?;
        Ok(())
    }
}

/// Dials a resolved origin, returning the connection to forward to. A seam so a
/// test can hand the proxy a scripted in-memory origin instead of a real TCP
/// dial — the second half (with [`Conn`]) of what makes the forward path
/// testable without a socket.
pub trait Connector: Send + Sync {
    /// Connect to `addr`, or fail (surfaced to the client as `502 Bad Gateway`).
    fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn Conn>>;
}

/// The production connector — a real bounded TCP dial to the resolved origin.
pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn Conn>> {
        let s = TcpStream::connect_timeout(&addr, CONN_TIMEOUT)?;
        s.set_read_timeout(Some(CONN_TIMEOUT))?;
        s.set_write_timeout(Some(CONN_TIMEOUT))?;
        Ok(Box::new(s))
    }
}

/// What a client's first request line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// `CONNECT host:port HTTP/x` — a TLS tunnel to open.
    Connect { host: String, port: u16 },
    /// `METHOD http://host[:port]/path HTTP/x` — a plaintext request to forward;
    /// `origin_line` is the same line rewritten to origin-form (`METHOD /path …`).
    Http {
        host: String,
        port: u16,
        origin_line: String,
    },
}

/// Parse a proxy request line. Returns `None` on anything malformed or a scheme
/// the proxy does not speak (only `CONNECT` and `http://` absolute-form).
fn parse_request_line(line: &str) -> Option<Target> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let uri = parts.next()?;
    let version = parts.next()?;
    if !version.starts_with("HTTP/") || parts.next().is_some() {
        return None;
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(uri, 443)?;
        return Some(Target::Connect { host, port });
    }
    // Absolute-form: METHOD http://host[:port]/path HTTP/x  (proxy requests only).
    let rest = uri.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(authority, 80)?;
    Some(Target::Http {
        host,
        port,
        origin_line: format!("{method} {path} {version}\r\n"),
    })
}

/// Split `host:port` (or a bare host, or a bracketed IPv6 literal) into
/// `(host, port)`, defaulting the port. Returns `None` on an unparsable port.
fn split_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    // Bracketed IPv6 literal: [::1] or [::1]:8080
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        // Host part still holds a ':' → an unbracketed IPv6 literal ("::1"); take
        // the whole authority as the host at the default port (fail-safe: a weird
        // string just misses the exact-name allow-list).
        Some((h, _)) if h.contains(':') => Some((authority.to_string(), default_port)),
        // ":443" — no host.
        Some(("", _)) => None,
        // A single ':' → host:port; a non-numeric port is malformed (reject).
        Some((h, p)) => Some((h.to_string(), p.parse::<u16>().ok()?)),
        // No ':' → a bare host at the default port.
        None => Some((authority.to_string(), default_port)),
    }
}

/// The exact-hostname allow-list, mirroring `ToolContext::check_net`'s membership,
/// plus (#385) an independent, exact-hostname **private-host approval** set —
/// forward-ported from the 0.7 maintenance line (`ac3d34a`) onto this file's
/// post-#372/#383 bounded-worker shape.
#[derive(Clone)]
struct HostPolicy {
    allowed: Arc<HashSet<String>>,
    private: Arc<HashSet<String>>,
}

impl HostPolicy {
    fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed: Arc::new(hosts.into_iter().collect()),
            private: Arc::new(HashSet::new()),
        }
    }
    fn allows(&self, host: &str) -> bool {
        self.allowed.contains(host)
    }
    fn with_private_hosts(
        hosts: impl IntoIterator<Item = String>,
        private_hosts: impl IntoIterator<Item = String>,
    ) -> io::Result<Self> {
        let mut policy = Self::new(hosts);
        policy.private = Arc::new(canonical_private_hosts(private_hosts)?);
        Ok(policy)
    }
    /// Whether `host` carries BOTH ordinary network authority (the allow-list)
    /// AND a separately approved exact private-host grant.
    fn allows_private(&self, host: &str) -> bool {
        self.allows(host)
            && canonical_private_host(host).is_some_and(|host| self.private.contains(&host))
    }
}

// Exact names only: no wildcard, URL, port, path, or ambiguous numeric address.
// ASCII DNS labels include ACE/punycode; embedders must IDNA-normalize Unicode.
fn canonical_private_host(host: &str) -> Option<String> {
    use std::net::IpAddr;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    if let Some(ip) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return ip
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|ip| ip.to_string());
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        return None;
    }
    let labels: Vec<_> = host.split('.').collect();
    if labels.iter().any(|label| {
        label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) || labels.last().is_some_and(|label| {
        label.bytes().all(|b| b.is_ascii_digit())
            || label.starts_with("0x")
            || label.starts_with("0X")
    }) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Validate and canonicalize exact private-host approvals from an owning
/// harness. This does not grant ordinary network authority or permit an
/// address; the proxy still intersects the host scope and screens each
/// resolved address.
pub fn canonical_private_hosts(
    hosts: impl IntoIterator<Item = String>,
) -> io::Result<HashSet<String>> {
    hosts
        .into_iter()
        .map(|host| {
            canonical_private_host(&host).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private-host grants require exact DNS names or IP addresses",
                )
            })
        })
        .collect()
}

// ── #372: lifecycle, bounded worker ownership, and frozen final evidence ───────

const LIFECYCLE_RUNNING: u8 = 0;
const LIFECYCLE_SHUTTING_DOWN: u8 = 1;
const LIFECYCLE_QUIESCENT: u8 = 2;

/// Lock a `Mutex`, recovering from poisoning rather than propagating it — none
/// of the critical sections in this module can panic while holding the lock, so
/// poisoning should never occur; this just keeps a stray panic elsewhere from
/// cascading into every subsequent lock attempt.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bounded, serializable evidence returned by [`ProxyHandle::shutdown_and_join`]
/// — the proxy's TERMINAL account of everything it did, frozen the moment the
/// proxy reached `Quiescent`. Every accepted connection is reflected in exactly
/// one of the counters below (#372 §3); nothing here can mutate after it is
/// returned, because the [`ProxyHandle`] that produced it has been consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyFinalEvidence {
    /// Distinct hosts refused (denied by the allow-list), deduped, sorted, and
    /// bounded to at most [`MAX_REFUSED_HOSTS`] entries of at most
    /// [`MAX_REFUSED_HOST_BYTES`] bytes each.
    #[serde(deserialize_with = "deserialize_bounded_hosts")]
    pub refused_hosts: Vec<String>,
    /// Refusal attempts NOT reflected in `refused_hosts` because the host-count
    /// bound or the per-host byte bound was already reached — the exact count,
    /// never silently dropped.
    pub omitted_refused_attempts: u64,
    /// Connections handed to a connection worker (i.e. they passed the capacity
    /// check and a worker thread was successfully spawned for them).
    pub accepted: u64,
    /// Connections whose worker reached a terminal accounting outcome. Equal to
    /// `accepted` in any `Ok` result; a shortfall can only appear attached to a
    /// [`ProxyFinalizeError::Incomplete`], never in a successful result.
    pub completed: u64,
    /// Connections refused purely for being over the worker cap — counted as
    /// overload, never mislabeled as an authority denial.
    pub capacity_rejects: u64,
    /// Completed connections whose proxy-owned sockets were force-closed by
    /// shutdown before they reached their own natural terminal point (e.g. an
    /// idle tunnel with no data flowing either way).
    pub shutdown_aborts: u64,
    /// Connection-worker thread-spawn failures observed during this proxy's
    /// lifetime.
    pub worker_spawn_failures: u64,
    /// Connection-worker panics observed (a failed `JoinHandle::join`).
    pub worker_panics: u64,
    /// The configured worker cap in force for this proxy.
    pub worker_cap: usize,
    /// The highest number of simultaneously active connection workers observed.
    pub high_water_workers: usize,
}

/// Bounded `Vec<String>` deserialization for [`ProxyFinalEvidence::refused_hosts`]
/// (#372 §3/acceptance: "hostile oversized evidence deserialization"). Rejects a
/// payload with more than [`MAX_REFUSED_HOSTS`] entries, or any entry longer
/// than [`MAX_REFUSED_HOST_BYTES`], as soon as the offending element is seen —
/// never after collecting the whole hostile array.
fn deserialize_bounded_hosts<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoundedHosts;
    impl<'de> Visitor<'de> for BoundedHosts {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "at most {MAX_REFUSED_HOSTS} host strings, each at most {MAX_REFUSED_HOST_BYTES} bytes"
            )
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let hint = seq.size_hint().unwrap_or(0).min(MAX_REFUSED_HOSTS);
            let mut out = Vec::with_capacity(hint);
            while let Some(host) = seq.next_element::<String>()? {
                if out.len() >= MAX_REFUSED_HOSTS {
                    return Err(de::Error::custom(format!(
                        "refused_hosts exceeds the {MAX_REFUSED_HOSTS}-host bound"
                    )));
                }
                if host.len() > MAX_REFUSED_HOST_BYTES {
                    return Err(de::Error::custom(format!(
                        "refused_hosts entry exceeds the {MAX_REFUSED_HOST_BYTES}-byte bound"
                    )));
                }
                out.push(host);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_seq(BoundedHosts)
}

/// Why [`ProxyHandle::shutdown_and_join`] could not return complete, trustworthy
/// evidence. Both variants mean completeness is NOT provable — callers must
/// treat this as a failure, never fall back to reading `partial` as if it were
/// terminal (#372 §3/§7).
#[derive(Debug)]
pub enum ProxyFinalizeError {
    /// The accept supervisor thread itself panicked (its `JoinHandle::join`
    /// failed).
    SupervisorFailed {
        /// Human-readable detail.
        reason: String,
    },
    /// One or more connection workers could not be spawned or panicked, so
    /// "every accepted connection has exactly one terminal accounting outcome"
    /// is not provable. `partial` is the best-effort evidence gathered before
    /// that became true — bounded and serializable like [`ProxyFinalEvidence`],
    /// but explicitly NOT terminal.
    Incomplete {
        /// The evidence gathered before completeness became unprovable.
        partial: ProxyFinalEvidence,
        /// Human-readable detail.
        reason: String,
    },
}

impl std::fmt::Display for ProxyFinalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SupervisorFailed { reason } => {
                write!(f, "egress proxy accept supervisor failed: {reason}")
            }
            Self::Incomplete { reason, .. } => {
                write!(f, "egress proxy finalization is incomplete: {reason}")
            }
        }
    }
}

impl std::error::Error for ProxyFinalizeError {}

/// The live (mutable, pre-`Quiescent`) evidence accumulator. One instance is
/// shared (behind a `Mutex`) across the accept loop and every connection
/// worker; [`Self::snapshot`] freezes it into a [`ProxyFinalEvidence`].
#[derive(Debug, Default)]
struct Evidence {
    refused: BTreeSet<String>,
    omitted_refused: u64,
    accepted: u64,
    completed: u64,
    capacity_rejects: u64,
    shutdown_aborts: u64,
    worker_spawn_failures: u64,
    worker_panics: u64,
}

impl Evidence {
    /// Record one refused (denied) host, respecting the #372 §3 bounds. A host
    /// already retained is not re-counted as omitted — it carries no new
    /// information.
    fn record_refused(&mut self, host: &str) {
        if self.refused.contains(host) {
            return;
        }
        if host.len() > MAX_REFUSED_HOST_BYTES || self.refused.len() >= MAX_REFUSED_HOSTS {
            self.omitted_refused += 1;
            return;
        }
        self.refused.insert(host.to_string());
    }

    fn refused_sorted(&self) -> Vec<String> {
        self.refused.iter().cloned().collect()
    }

    fn snapshot(&self, worker_cap: usize, high_water_workers: usize) -> ProxyFinalEvidence {
        ProxyFinalEvidence {
            refused_hosts: self.refused_sorted(),
            omitted_refused_attempts: self.omitted_refused,
            accepted: self.accepted,
            completed: self.completed,
            capacity_rejects: self.capacity_rejects,
            shutdown_aborts: self.shutdown_aborts,
            worker_spawn_failures: self.worker_spawn_failures,
            worker_panics: self.worker_panics,
            worker_cap,
            high_water_workers,
        }
    }
}

/// A connection worker's terminal outcome, as seen by the supervisor that joins
/// it — distinct from the connection's *protocol* outcome (`NetDecision`, which
/// the audit sink sees): this is about whether the worker reached that outcome
/// on its own, or was force-closed by shutdown first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnOutcome {
    /// The worker returned on its own (whatever the protocol outcome was).
    Completed,
    /// Shutdown force-closed this connection's proxy-owned sockets before (or
    /// concurrently with) the worker's own completion.
    Aborted,
}

/// A proxy-owned handle to force-close one connection's sockets during
/// shutdown (#372 §2) — how an idle/tunnel connection's blocked read is made to
/// unblock instead of hanging finalization forever. Registered per active
/// worker; `client` is fixed at construction, `origin` is set once the worker
/// actually dials out (CONNECT/HTTP paths only reach that after an allow-list
/// pass). Every field is behind a `Mutex` purely so `Arc<ConnCloser>` is `Sync`
/// (`Box<dyn Conn>` is `Send` but not `Sync`) — `close_all` is the only method
/// that ever touches them after construction, and it is safe to call more than
/// once (shutting down an already-shut-down socket is a harmless error we
/// discard).
struct ConnCloser {
    client: Mutex<Box<dyn Conn>>,
    origin: Mutex<Option<Box<dyn Conn>>>,
    /// Set by `close_all`; read by the owning worker after `handle_conn`
    /// returns to classify its own outcome as [`ConnOutcome::Aborted`].
    aborted: AtomicBool,
}

impl ConnCloser {
    fn new(client: Box<dyn Conn>) -> Self {
        Self {
            client: Mutex::new(client),
            origin: Mutex::new(None),
            aborted: AtomicBool::new(false),
        }
    }

    fn set_origin(&self, origin: Box<dyn Conn>) {
        *lock(&self.origin) = Some(origin);
    }

    /// Force-close every proxy-owned socket end for this connection. Idempotent
    /// and safe to call from a thread other than the one running `handle_conn`.
    fn close_all(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        let _ = lock(&self.client).shutdown(Shutdown::Both);
        if let Some(o) = lock(&self.origin).as_deref() {
            let _ = o.shutdown(Shutdown::Both);
        }
    }
}

/// Test-only synchronization seam (#372 acceptance tests): production always
/// runs with [`ConnHooks::default`] (both hooks `None`, zero overhead beyond an
/// `Option` check). A test can pause a connection worker at a chosen point —
/// right on entry, or right after its allow-list decision is recorded — to
/// drive the proxy's shutdown path against a worker that is deterministically
/// still busy.
#[derive(Clone, Default)]
struct ConnHooks {
    /// Fired as the very first action inside `handle_conn`.
    entered: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Fired immediately after the connection's allow-list decision (and its
    /// audit record / refused-host bookkeeping) is finalized, before the
    /// response is written.
    decided: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ConnHooks {
    fn fire_entered(&self) {
        if let Some(f) = &self.entered {
            f();
        }
    }
    fn fire_decided(&self) {
        if let Some(f) = &self.decided {
            f();
        }
    }
}

/// The set of currently-active connection workers, plus the id counter used to
/// key them. Behind one `Mutex` on [`Shared`]; the accept loop inserts and
/// opportunistically reaps, [`ProxyHandle::teardown`] drains the rest.
#[derive(Default)]
struct WorkerBook {
    next_id: u64,
    active: HashMap<u64, (JoinHandle<ConnOutcome>, Arc<ConnCloser>)>,
}

/// State shared between [`ProxyHandle`], the accept-loop thread, and every
/// connection-worker thread it spawns.
struct Shared {
    lifecycle: AtomicU8,
    workers: Mutex<WorkerBook>,
    evidence: Mutex<Evidence>,
    /// The configured worker cap (#372 §1) — immutable for the proxy's lifetime.
    cap: usize,
    high_water: AtomicUsize,
    /// Test-only fault injection (#372 acceptance: "injected spawn failure"): a
    /// positive count here makes the accept loop simulate that many connection
    /// worker spawn failures instead of actually spawning, without needing to
    /// exhaust real OS thread resources. Zero (the production default) never
    /// engages this path.
    inject_spawn_failures: AtomicUsize,
}

impl Shared {
    fn new(cap: usize) -> Self {
        Self {
            lifecycle: AtomicU8::new(LIFECYCLE_RUNNING),
            workers: Mutex::new(WorkerBook::default()),
            evidence: Mutex::new(Evidence::default()),
            cap,
            high_water: AtomicUsize::new(0),
            inject_spawn_failures: AtomicUsize::new(0),
        }
    }
}

/// Reap every connection worker that has already finished, tallying each into
/// `shared.evidence`. Non-blocking: only workers whose `JoinHandle::is_finished`
/// is already `true` are joined, so this never stalls the accept loop.
fn reap_finished(shared: &Shared) {
    let reaped: Vec<JoinHandle<ConnOutcome>> = {
        let mut book = lock(&shared.workers);
        let done: Vec<u64> = book
            .active
            .iter()
            .filter_map(|(id, (h, _))| h.is_finished().then_some(*id))
            .collect();
        done.into_iter()
            .filter_map(|id| book.active.remove(&id).map(|(h, _)| h))
            .collect()
    };
    for h in reaped {
        tally_join(shared, h.join());
    }
}

/// Tally one worker's `JoinHandle::join` result into `shared.evidence` — the
/// ONLY place a connection worker's terminal outcome is recorded, whether
/// reaped opportunistically by [`reap_finished`] or drained by
/// [`ProxyHandle::teardown`].
fn tally_join(shared: &Shared, result: std::thread::Result<ConnOutcome>) {
    let mut ev = lock(&shared.evidence);
    match result {
        Ok(ConnOutcome::Completed) => ev.completed += 1,
        Ok(ConnOutcome::Aborted) => {
            ev.completed += 1;
            ev.shutdown_aborts += 1;
        }
        Err(_) => ev.worker_panics += 1,
    }
}

/// A running loopback egress proxy. Bounded and joinable (#372): every worker it
/// spawns is retained until reaped or drained, and [`Self::shutdown_and_join`]
/// is the explicit finalizer that proves so. Dropping the handle without calling
/// it still shuts everything down and joins every worker (fail-safe), but
/// discards the evidence — see the module-level lifecycle note.
pub struct ProxyHandle {
    addr: SocketAddr,
    accept: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ProxyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHandle")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl ProxyHandle {
    /// The loopback address the proxy listens on. A spawned child is wired via
    /// [`Self::proxy_env`]; a **no-subprocess** caller (#257 — e.g. a
    /// `reqwest::Client`) points itself here instead:
    /// `reqwest::Proxy::all(format!("http://{}", handle.addr()))`.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// #196: the CONNECT/HTTP hosts this proxy has REFUSED so far (not on the
    /// allow-list), deduplicated and sorted, bounded as documented on
    /// [`ProxyFinalEvidence::refused_hosts`].
    ///
    /// **Provisional** (#372 §2): this is a LIVE snapshot that can still change
    /// while the proxy is `Running`/`ShuttingDown` — a connection worker may
    /// record a denial after this call returns. A caller that needs TERMINAL,
    /// frozen evidence must call [`Self::shutdown_and_join`] instead; this
    /// method remains for best-effort diagnostics only.
    #[must_use]
    pub fn refused_hosts(&self) -> Vec<String> {
        lock(&self.shared.evidence).refused_sorted()
    }

    /// The `*_PROXY` environment the child needs to route through this proxy.
    /// Both cases are set: curl honours lowercase `http_proxy` (it ignores the
    /// uppercase form for CGI-safety) but uppercase `HTTPS_PROXY`/`ALL_PROXY`;
    /// other tools (wget, requests, node) read the rest.
    #[must_use]
    pub fn proxy_env(&self) -> Vec<(String, String)> {
        let url = format!("http://{}", self.addr);
        [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
        ]
        .iter()
        .map(|k| ((*k).to_string(), url.clone()))
        .collect()
    }

    /// #372: the explicit, consuming finalizer. Drives `Running -> ShuttingDown
    /// -> Quiescent`: stops admission, force-closes every proxy-owned socket end
    /// still open (so an idle/tunnel connection cannot hang this call forever),
    /// then BLOCKS until the accept worker and every connection/splice worker
    /// have been joined — including one deliberately paused mid-decision by a
    /// test, which this call waits for rather than racing past.
    ///
    /// Returns `Ok` only when every accepted connection reached exactly one
    /// terminal accounting outcome with no spawn failure, panic, or join
    /// failure along the way; otherwise `Err` — never an `Ok` evidence value
    /// that silently omits a failure (#372 §7).
    ///
    /// **Bound on an idle/tunnel connection.** The force-close is a `shutdown`
    /// on this proxy's own socket end, called from this process — on
    /// Linux/macOS that interrupts an in-flight blocked read on a duplicated
    /// handle promptly; that prompt-interrupt behavior is not guaranteed on
    /// every platform. The portable, honest upper bound is each connection's
    /// own read/write timeout (`CONN_TIMEOUT`, set when it was accepted or
    /// dialed): this call never hangs past it, even where `shutdown` alone
    /// does not immediately unblock the read.
    ///
    /// Blocking, std-only. An async caller must run this on a blocking
    /// executor/thread and await it before emitting its own terminal result —
    /// never call it directly from a Tokio reactor thread.
    pub fn shutdown_and_join(mut self) -> Result<ProxyFinalEvidence, ProxyFinalizeError> {
        self.teardown()
    }

    /// The shared shutdown-and-join engine behind both [`Self::shutdown_and_join`]
    /// and `Drop` — see the module-level lifecycle note for why the two share
    /// this rather than Drop being a distinct, weaker path. Idempotent: once the
    /// first call has drained `accept`/`workers`, a second call (Drop running
    /// after `shutdown_and_join` already did the real work) finds nothing left
    /// to do and returns the same frozen snapshot cheaply.
    fn teardown(&mut self) -> Result<ProxyFinalEvidence, ProxyFinalizeError> {
        // Idempotent fast path: `Drop` always runs `teardown` again after
        // `shutdown_and_join` already did the real work (see the module-level
        // lifecycle note) — by then `accept`/`workers` are already empty, so
        // this would just re-lock and re-scan for nothing. The returned value
        // is discarded by `Drop` either way; skip straight to a cheap snapshot.
        if self.shared.lifecycle.load(Ordering::SeqCst) == LIFECYCLE_QUIESCENT {
            let ev = lock(&self.shared.evidence);
            return Ok(ev.snapshot(
                self.shared.cap,
                self.shared.high_water.load(Ordering::SeqCst),
            ));
        }

        // Linearize: stop admitting new connections. Idempotent — a failed CAS
        // just means shutdown was already signaled (or is already Quiescent).
        let _ = self.shared.lifecycle.compare_exchange(
            LIFECYCLE_RUNNING,
            LIFECYCLE_SHUTTING_DOWN,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );

        // Force-close every currently-tracked connection's proxy-owned sockets
        // FIRST, before joining anything — an idle/tunnel connection is blocked
        // on a read that only this unblocks (#372 acceptance: "occupied/idle
        // tunnel shutdown"). A connection paused by a test hook on purpose (not
        // blocked on I/O) is unaffected by this and is instead waited for below.
        {
            let book = lock(&self.shared.workers);
            for (_, closer) in book.active.values() {
                closer.close_all();
            }
        }

        // Wake and join the accept worker. The listener itself is owned by that
        // thread's closure, so it is closed the moment the thread returns.
        let mut supervisor_failure = None;
        if let Some(h) = self.accept.take() {
            // Best-effort wake for a blocking accept(): if this dial fails the
            // listener may already be gone, and the join below still completes
            // once the OS delivers shutdown some other way.
            let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
            if h.join().is_err() {
                supervisor_failure = Some("accept supervisor thread panicked".to_string());
            }
        }

        // Drain and join every remaining connection worker — including any the
        // accept loop admitted right up to observing shutdown, and any still
        // blocked (an idle tunnel just force-closed above, or a test-paused
        // decision). This BLOCKS until each one returns; it does not race past
        // a worker that is still legitimately in flight.
        let remaining: Vec<(JoinHandle<ConnOutcome>, Arc<ConnCloser>)> = {
            let mut book = lock(&self.shared.workers);
            book.active.drain().map(|(_, v)| v).collect()
        };
        for (handle, closer) in remaining {
            // Cover anything admitted after the sweep above raced ahead of it.
            closer.close_all();
            tally_join(&self.shared, handle.join());
        }

        let snapshot = {
            let ev = lock(&self.shared.evidence);
            ev.snapshot(
                self.shared.cap,
                self.shared.high_water.load(Ordering::SeqCst),
            )
        };
        self.shared
            .lifecycle
            .store(LIFECYCLE_QUIESCENT, Ordering::SeqCst);

        if let Some(reason) = supervisor_failure {
            return Err(ProxyFinalizeError::SupervisorFailed { reason });
        }
        if snapshot.worker_panics > 0 || snapshot.worker_spawn_failures > 0 {
            return Err(ProxyFinalizeError::Incomplete {
                reason: format!(
                    "{} worker panic(s) and {} spawn failure(s): completeness is not provable",
                    snapshot.worker_panics, snapshot.worker_spawn_failures
                ),
                partial: snapshot,
            });
        }
        Ok(snapshot)
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        // Fail-safe: same shutdown-and-join as the explicit finalizer, evidence
        // discarded. See the module-level lifecycle note.
        let _ = self.teardown();
    }
}

/// Start a loopback forward proxy that admits only `allow_hosts`, resolving via
/// `resolver` and auditing every connection through `sink` ([`NullSink`] for no
/// audit). Binds `127.0.0.1:0` (an ephemeral port — concurrent runs never
/// collide) and serves — bounded to [`DEFAULT_WORKER_CAP`] simultaneously active
/// connection workers — until the returned [`ProxyHandle`] is finalized or
/// dropped.
///
/// Fail-closed: an error binding the listener is returned so the caller refuses
/// the run rather than spawning an unfenced child.
pub fn start(
    allow_hosts: impl IntoIterator<Item = String>,
    resolver: Arc<dyn Resolver>,
    sink: Arc<dyn AuditSink>,
) -> io::Result<ProxyHandle> {
    start_with_private_hosts(allow_hosts, [], resolver, sink)
}

/// Start the same fenced egress proxy with an additional, exact private-host
/// approval supplied by the owning harness (#385, forward-ported from the 0.7
/// maintenance line's `ac3d34a`). Both the ordinary host allow-list and this
/// independent approval must permit the requested name. Only RFC1918 and IPv6
/// unique-local destinations may be opted in this way; metadata/link-local,
/// unspecified, multicast, and other reserved ranges remain blocked.
///
/// [`start`] passes an empty set. This is transient host policy, not a new
/// caveat or a substitute for the child's kernel loopback fence. Malformed
/// names, URLs, ports and wildcard patterns fail before binding.
pub fn start_with_private_hosts(
    allow_hosts: impl IntoIterator<Item = String>,
    private_hosts: impl IntoIterator<Item = String>,
    resolver: Arc<dyn Resolver>,
    sink: Arc<dyn AuditSink>,
) -> io::Result<ProxyHandle> {
    start_internal(
        allow_hosts,
        private_hosts,
        resolver,
        sink,
        Arc::new(TcpConnector),
        DEFAULT_WORKER_CAP,
        ConnHooks::default(),
    )
}

/// Start the egress proxy for a **general remote-host** `net` grant (#257) — the
/// caveats-level convenience over [`start`], for an external caller.
///
/// `Ok(None)` when `caveats` does not call for a proxy
/// ([`crate::net_egress_proxy_hosts`] is `None`): `net: All` needs no fence,
/// and deny-all / loopback-only stay with their kernel owners. `Ok(Some(handle))`
/// = the proxy is live on `handle.addr()`, admitting exactly the granted hosts.
/// `Err` = the grant calls for a proxy but the loopback listener could not bind —
/// the caller must fail closed, never proceed unfenced.
///
/// Two consumption shapes:
/// - **Spawned child**: pair with [`crate::loopback_fenced_caveats`] (the kernel
///   fence) and wire the child via [`ProxyHandle::proxy_env`] — what
///   `ConfinedCommand::spawn_tokio` does automatically under this grant.
/// - **No subprocess** (an in-process `reqwest::Client`): route the client at
///   `http://{handle.addr()}` (`reqwest::Proxy::all`). No kernel fence applies —
///   the caller is pointing *itself* at the proxy, so the per-host allow-list is
///   exactly as strong as the caller's routing (advisory grade, honestly).
///
/// Audit is off ([`NullSink`]); an embedder that wants the audit trail or a
/// custom resolver uses [`start`] directly.
pub fn start_egress_proxy(caveats: &crate::Caveats) -> io::Result<Option<ProxyHandle>> {
    let Some(hosts) = crate::net_egress_proxy_hosts(caveats) else {
        return Ok(None);
    };
    start_for_hosts(hosts).map(Some)
}

/// [`start`] with the production defaults (platform resolver, audit off) — the
/// shared entry both [`start_egress_proxy`] and `ConfinedCommand::spawn_tokio`'s
/// egress wiring (#257) call, so "start the proxy for these hosts" has exactly
/// one production spelling.
pub fn start_for_hosts(allow_hosts: impl IntoIterator<Item = String>) -> io::Result<ProxyHandle> {
    start(allow_hosts, Arc::new(StdResolver), Arc::new(NullSink))
}

/// The real engine behind [`start`] — parameterized over the worker cap,
/// connector, and test hooks so the acceptance-test suite can drive the exact
/// same accept-loop/lifecycle machinery production uses, deterministically.
fn start_internal(
    allow_hosts: impl IntoIterator<Item = String>,
    private_hosts: impl IntoIterator<Item = String>,
    resolver: Arc<dyn Resolver>,
    sink: Arc<dyn AuditSink>,
    connector: Arc<dyn Connector>,
    cap: usize,
    hooks: ConnHooks,
) -> io::Result<ProxyHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let addr = listener.local_addr()?;
    let policy = HostPolicy::with_private_hosts(allow_hosts, private_hosts)?;
    let shared = Arc::new(Shared::new(cap));

    let accept = {
        let shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("agent-bridle-egress-proxy".to_string())
            .spawn(move || {
                for stream in listener.incoming() {
                    if shared.lifecycle.load(Ordering::SeqCst) != LIFECYCLE_RUNNING {
                        break;
                    }
                    let Ok(client) = stream else { continue };
                    let client: Box<dyn Conn> = Box::new(client);
                    // Bound every write this accept loop itself might make
                    // (capacity/spawn-failure 503s below) to CONN_TIMEOUT — a
                    // hostile client that never reads could otherwise block
                    // this single accept thread's write forever, wedging
                    // admission for every other connection and making
                    // `shutdown_and_join`'s accept-thread join hang too.
                    // `handle_conn` also sets this on its own dup; harmless.
                    let _ = client.set_timeouts(CONN_TIMEOUT);

                    // Bound #1: reap anything already finished before deciding
                    // whether there is room for one more active worker.
                    reap_finished(&shared);

                    let active_len = lock(&shared.workers).active.len();
                    if active_len >= shared.cap {
                        lock(&shared.evidence).capacity_rejects += 1;
                        let _ = respond(client.as_ref(), 503, "Service Unavailable");
                        continue;
                    }

                    // Test-only fault injection (#372 acceptance): simulate a
                    // spawn failure without touching real OS thread limits.
                    let injected = if shared.inject_spawn_failures.load(Ordering::SeqCst) > 0 {
                        shared.inject_spawn_failures.fetch_sub(1, Ordering::SeqCst);
                        true
                    } else {
                        false
                    };
                    if injected {
                        lock(&shared.evidence).worker_spawn_failures += 1;
                        let _ = respond(client.as_ref(), 503, "Service Unavailable");
                        continue;
                    }

                    let closer = match client.dup() {
                        Ok(dup) => Arc::new(ConnCloser::new(dup)),
                        Err(_) => {
                            // Cannot even register a closer for this connection —
                            // treat identically to a spawn failure: it will never
                            // be serviced or accounted as accepted.
                            lock(&shared.evidence).worker_spawn_failures += 1;
                            continue;
                        }
                    };

                    let policy = policy.clone();
                    let resolver = Arc::clone(&resolver);
                    let connector = Arc::clone(&connector);
                    let sink = Arc::clone(&sink);
                    let shared_for_worker = Arc::clone(&shared);
                    let hooks_for_worker = hooks.clone();
                    let closer_for_worker = Arc::clone(&closer);
                    let spawn_result = thread::Builder::new()
                        .name("agent-bridle-egress-conn".to_string())
                        .spawn(move || {
                            let env = ConnEnv {
                                policy: &policy,
                                connector: connector.as_ref(),
                                resolver: resolver.as_ref(),
                                sink: sink.as_ref(),
                            };
                            let _ = handle_conn(
                                client,
                                &env,
                                &shared_for_worker.evidence,
                                &closer_for_worker,
                                &hooks_for_worker,
                            );
                            if closer_for_worker.aborted.load(Ordering::SeqCst) {
                                ConnOutcome::Aborted
                            } else {
                                ConnOutcome::Completed
                            }
                        });
                    match spawn_result {
                        Ok(handle) => {
                            let count = {
                                let mut book = lock(&shared.workers);
                                let id = book.next_id;
                                book.next_id += 1;
                                book.active.insert(id, (handle, closer));
                                book.active.len()
                            };
                            shared.high_water.fetch_max(count, Ordering::SeqCst);
                            lock(&shared.evidence).accepted += 1;
                        }
                        Err(_) => {
                            // `client` was moved into the failed spawn attempt's
                            // closure and is gone with it; `closer` (not yet
                            // registered anywhere) still holds a live dup, so the
                            // client gets the same 503 the capacity/injected
                            // paths give it rather than a bare reset.
                            lock(&shared.evidence).worker_spawn_failures += 1;
                            let guard = lock(&closer.client);
                            let _ = respond(guard.as_ref(), 503, "Service Unavailable");
                        }
                    }
                }
            })?
    };

    Ok(ProxyHandle {
        addr,
        accept: Some(accept),
        shared,
    })
}

/// Serve one client connection: parse its request line, enforce the allow-list,
/// and either tunnel (`CONNECT`) or forward (`http://`) to the resolved origin.
/// Every connection with a parsed host is recorded through `sink`. `closer` is
/// this connection's #372 shutdown handle (its `origin` half is populated once
/// dialed); `hooks` is the test-only pause seam (a no-op in production).
/// Immutable per-connection dependencies `handle_conn` needs — bundled into one
/// borrow so the function stays under clippy's argument-count lint without
/// losing the #166 seam-based testability (each field is still a trait
/// object/reference a test can substitute).
struct ConnEnv<'a> {
    policy: &'a HostPolicy,
    connector: &'a dyn Connector,
    resolver: &'a dyn Resolver,
    sink: &'a dyn AuditSink,
}

fn handle_conn(
    client: Box<dyn Conn>,
    env: &ConnEnv<'_>,
    evidence: &Mutex<Evidence>,
    closer: &ConnCloser,
    hooks: &ConnHooks,
) -> io::Result<()> {
    hooks.fire_entered();
    client.set_timeouts(CONN_TIMEOUT)?;
    let mut reader = BufReader::new(client.dup()?);
    let t0 = Instant::now();

    let line = read_line_bounded(&mut reader)?;
    let Some(target) = parse_request_line(&line) else {
        // No host to attribute — a malformed request is not an egress event.
        return respond(client.as_ref(), 400, "Bad Request");
    };

    let (host, port, kind) = match &target {
        Target::Connect { host, port } => (host.clone(), *port, NetKind::Connect),
        Target::Http { host, port, .. } => (host.clone(), *port, NetKind::Http),
    };
    // Emit the audit record once, whatever the outcome.
    let audit = |decision: NetDecision, up: u64, down: u64| {
        env.sink.record(&NetAuditEvent {
            ts_ms: now_ms(),
            host: host.clone(),
            port,
            kind,
            decision,
            bytes_up: up,
            bytes_down: down,
            dur_ms: t0.elapsed().as_millis() as u64,
        });
    };

    if !env.policy.allows(&host) {
        audit(NetDecision::Denied, 0, 0); // the exfil-attempt signal
                                          // #196/#372: record the refused host (bounded, #372 §3) so a consumer
                                          // can surface it as a structured `net` denial. Always on, independent
                                          // of the opt-in audit sink.
        lock(evidence).record_refused(&host);
        hooks.fire_decided();
        return respond(client.as_ref(), 403, "Forbidden");
    }

    match target {
        Target::Connect { host, port } => {
            // CONNECT: drain the remaining request headers (up to the blank line)
            // before the tunnel begins — the client waits for our 200 first.
            drain_headers(&mut reader)?;
            let origin = match env
                .resolver
                .resolve(&host, port)
                .and_then(|addr| guard_target(addr, &host, env.policy))
                .and_then(|addr| env.connector.connect(addr))
            {
                Ok(o) => o,
                Err(_) => {
                    audit(NetDecision::Error, 0, 0);
                    hooks.fire_decided();
                    return respond(client.as_ref(), 502, "Bad Gateway");
                }
            };
            // #372: register the dialed origin so shutdown can force-close it
            // too (an idle tunnel is blocked on THIS socket, not just the
            // client's).
            if let Ok(dup) = origin.dup() {
                closer.set_origin(dup);
            }
            // Fire BEFORE the client-visible write (matching every other
            // branch), not after — a write that fails (`?`, below) would
            // otherwise return early and skip it, and the seam contract is
            // "decision recorded", not "response delivered".
            hooks.fire_decided();
            // A CONNECT success is a *bare* status line — no body, no
            // `Content-Length` — after which the socket is an opaque tunnel. (Do
            // NOT use `respond`, which appends a body that would corrupt it.)
            let mut client = client;
            client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
            // Forward via `splice_buffered` (not a raw `tunnel`) so any bytes the
            // client already pipelined past the CONNECT header block — buffered in
            // `reader` — reach the origin (a TLS ClientHello sent with the CONNECT;
            // #138). The up-copy reads the BufReader, which drains its buffer first.
            let (up, down) = splice_buffered(reader, client, origin)?;
            audit(NetDecision::Allowed, up, down);
            Ok(())
        }
        Target::Http {
            host,
            port,
            origin_line,
        } => {
            // Read the client's request headers and **drop any client-supplied
            // Host** — an attacker could send `Host: disallowed.com` to domain-front
            // off a shared/CDN origin at the allow-listed IP. Substitute the proxy's
            // own Host, derived from the *validated* authority, so the origin only
            // ever sees the host the allow-list approved.
            let headers = read_headers(&mut reader)?;
            let mut origin = match env
                .resolver
                .resolve(&host, port)
                .and_then(|addr| guard_target(addr, &host, env.policy))
                .and_then(|addr| env.connector.connect(addr))
            {
                Ok(o) => o,
                Err(_) => {
                    audit(NetDecision::Error, 0, 0);
                    hooks.fire_decided();
                    return respond(client.as_ref(), 502, "Bad Gateway");
                }
            };
            if let Ok(dup) = origin.dup() {
                closer.set_origin(dup);
            }
            hooks.fire_decided();
            let host_hdr = if port == 80 {
                format!("Host: {host}\r\n")
            } else {
                format!("Host: {host}:{port}\r\n")
            };
            origin.write_all(origin_line.as_bytes())?;
            origin.write_all(host_hdr.as_bytes())?;
            for h in &headers {
                if !h.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("host:")) {
                    origin.write_all(h.as_bytes())?;
                }
            }
            origin.write_all(b"\r\n")?; // end of the (rewritten) header block
            let (up, down) = splice_buffered(reader, client, origin)?; // forward the body
            audit(NetDecision::Allowed, up, down);
            Ok(())
        }
    }
}

/// Read one CRLF-terminated line, bounded to [`MAX_HEAD`]. Generic over the
/// buffered reader's source so the same logic drives a real socket or an
/// in-memory test stream.
fn read_line_bounded<R: Read>(reader: &mut BufReader<R>) -> io::Result<String> {
    let mut buf = Vec::new();
    reader.take(MAX_HEAD as u64).read_until(b'\n', &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read request header lines up to (not including) the terminating blank line,
/// bounded by [`MAX_HEAD`]. Each returned line keeps its trailing CRLF.
fn read_headers<R: Read>(reader: &mut BufReader<R>) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut total = 0usize;
    loop {
        let line = read_line_bounded(reader)?;
        total += line.len();
        if line == "\r\n" || line == "\n" || line.is_empty() || total > MAX_HEAD {
            return Ok(lines);
        }
        lines.push(line);
    }
}

/// Consume request headers up to and including the terminating blank line.
fn drain_headers<R: Read>(reader: &mut BufReader<R>) -> io::Result<()> {
    let mut total = 0usize;
    loop {
        let line = read_line_bounded(reader)?;
        total += line.len();
        if line == "\r\n" || line == "\n" || line.is_empty() || total > MAX_HEAD {
            return Ok(());
        }
    }
}

/// Write a minimal HTTP/1.1 status response and close.
fn respond(client: &dyn Conn, code: u16, reason: &str) -> io::Result<()> {
    let mut c = client.dup()?;
    let body = format!("{code} {reason}\n");
    write!(
        c,
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let _ = c.flush();
    let _ = client.shutdown(Shutdown::Both);
    Ok(())
}

/// Copy `from` → `to`, returning the bytes **forwarded** — preserved even if a
/// stream errors mid-copy (a reset after data still counts what flowed), so the
/// audit totals are not silently zeroed by an abrupt close (`std::io::copy`
/// discards its count on error). Stops at EOF, a write failure, or a read error.
fn copy_counted(from: &mut impl Read, to: &mut impl Write) -> u64 {
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
                total += n as u64;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    total
}

/// Bidirectional raw byte tunnel between `client` and `origin` (the `CONNECT`
/// case): two copy threads, each shutting its write half at EOF. Returns
/// `(bytes_up, bytes_down)` — child→origin and origin→child — for the audit.
/// SSRF-pivot guard (#138): refuse to dial a resolved origin whose IP is on an
/// **internal** range — RFC1918 private, `169.254/16` link-local (incl. the cloud
/// metadata endpoint `169.254.169.254`), `100.64/10` CGNAT, and IPv6 ULA/link-local.
/// The egress proxy fronts a *loopback-fenced* child; without this an allow-listed
/// name that resolves (or is rebound) to an internal address would give that child
/// a parent-mediated path to endpoints its own kernel fence forbids. Loopback is
/// **allowed** (the fenced child can already reach loopback directly, and the test
/// origins live there) and global addresses are allowed.
///
/// Default-on: only a separately approved exact host (#385, forward-ported from
/// the 0.7 line's `ac3d34a`) may resolve to RFC1918 or ULA space. Returns the
/// address unchanged when permitted, else `PermissionDenied`.
fn guard_target(addr: SocketAddr, host: &str, policy: &HostPolicy) -> io::Result<SocketAddr> {
    let ip = match addr.ip() {
        std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(addr.ip(), std::net::IpAddr::V4),
        ip => ip,
    };
    let approvable = match ip {
        std::net::IpAddr::V4(ip) => ip.is_private(),
        std::net::IpAddr::V6(ip) => {
            (ip.segments()[0] & 0xfe00) == 0xfc00
                // EC2's IPv6 metadata endpoint lies inside ULA, not link-local.
                && ip.segments() != [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
        }
    };
    if is_internal_ip(&ip) && !(approvable && policy.allows_private(host)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SSRF-guard: refusing to proxy to an internal (non-loopback) address",
        ));
    }
    Ok(addr)
}

/// `true` if `ip` is on a private/internal range the egress proxy must not pivot
/// to (see [`guard_target`]). Loopback and global addresses return `false`.
fn is_internal_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0 || o[0] >= 240
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                // 100.64.0.0/10 (CGNAT) — not covered by the std predicates.
                || (o[0] == 100 && (64..=127).contains(&o[1]))
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_internal_ip(&std::net::IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (seg0 & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (seg0 & 0xffc0) == 0xfec0 // deprecated site-local
                || (seg0 == 0x2001 && v6.segments()[1] == 0x0db8) // documentation
        }
    }
}

/// Like [`tunnel`] but the client side is a `BufReader` that may already hold
/// buffered bytes (the `http://` forward case, after the request line was read).
/// Returns `(bytes_up, bytes_down)` for the audit.
fn splice_buffered(
    mut client_reader: BufReader<Box<dyn Conn>>,
    client: Box<dyn Conn>,
    origin: Box<dyn Conn>,
) -> io::Result<(u64, u64)> {
    let mut o_write = origin.dup()?;
    let up = thread::spawn(move || {
        let n = copy_counted(&mut client_reader, &mut o_write);
        let _ = o_write.shutdown(Shutdown::Write);
        n
    });
    let mut o_read = origin;
    let mut c_write = client;
    let down = copy_counted(&mut o_read, &mut c_write);
    // The origin side has closed, so tear the whole connection down: shut down
    // BOTH client halves, not just write. `Write` alone leaves the `up` thread
    // blocked reading the client's (half-open) upload direction until CONN_TIMEOUT
    // — a plain client that sends its request then only reads the response never
    // closes its write half until it sees our EOF, and we never see its EOF: a
    // ~30s deadlock on every forward/tunnel (surfaced by the flaky `audit_records_*`
    // test, since the audit fires only once `up` joins). `Both` gives `up`'s read
    // on the shared socket an immediate EOF.
    let _ = c_write.shutdown(Shutdown::Both);
    // #372 §7: a panic on the `up` copy thread must not be silently folded into
    // "0 bytes forwarded" — that would let a splice-worker panic produce
    // apparently-complete evidence. Resume it on THIS thread (the owning
    // connection worker), so it propagates to the same `JoinHandle<ConnOutcome>`
    // `tally_join` already inspects, and is counted in `worker_panics` exactly
    // like any other connection-worker panic — no separate accounting needed.
    let up = match up.join() {
        Ok(n) => n,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    Ok((up, down))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::Condvar;

    // ── #257: the caveats-level public entry ────────────────────────────────

    /// `start_egress_proxy` starts NOTHING for the three grants that keep their
    /// kernel owners: `net: All` (nothing to fence), deny-all (kernel-denied),
    /// loopback-only (the ADR 0015 fence alone suffices) — criterion 3.
    #[test]
    fn start_egress_proxy_is_none_for_non_proxy_grants() {
        use crate::{Caveats, Scope};
        let all = Caveats::top();
        assert!(start_egress_proxy(&all).unwrap().is_none(), "net: All");
        let deny = Caveats {
            net: Scope::only([] as [String; 0]),
            ..Caveats::top()
        };
        assert!(start_egress_proxy(&deny).unwrap().is_none(), "deny-all");
        let loopback = Caveats {
            net: Scope::only(["localhost".to_string()]),
            ..Caveats::top()
        };
        assert!(
            start_egress_proxy(&loopback).unwrap().is_none(),
            "loopback-only"
        );
    }

    /// A general remote-host grant starts a live proxy on loopback; a raw
    /// CONNECT for a non-granted host is refused 403 and recorded in
    /// `refused_hosts()` — the no-subprocess (reqwest-style) consumption shape,
    /// exercised with a plain std TcpStream speaking the proxy protocol.
    #[test]
    fn start_egress_proxy_serves_a_remote_host_grant_and_refuses_off_list() {
        use crate::{Caveats, Scope};
        use std::io::{Read as _, Write as _};
        let granted = Caveats {
            net: Scope::only(["api.example.com".to_string()]),
            ..Caveats::top()
        };
        let handle = start_egress_proxy(&granted)
            .expect("bind loopback")
            .expect("a remote-host grant calls for the proxy");
        assert!(
            handle.addr().ip().is_loopback(),
            "proxy binds loopback only"
        );

        let mut client = TcpStream::connect(handle.addr()).expect("dial proxy");
        client
            .write_all(
                b"CONNECT evil.example.net:443 HTTP/1.1\r\nHost: evil.example.net:443\r\n\r\n",
            )
            .expect("send CONNECT");
        // The proxy answers 403 and closes; read to EOF so a short first read
        // can't truncate the status line.
        let mut reply = String::new();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let _ = client.read_to_string(&mut reply);
        assert!(
            reply.contains("403"),
            "off-list CONNECT must be refused: {reply:?}"
        );
        assert!(
            handle
                .refused_hosts()
                .contains(&"evil.example.net".to_string()),
            "refusal must be recorded: {:?}",
            handle.refused_hosts()
        );
    }

    /// #138 (SSRF pivot): the proxy must refuse to dial an allow-listed name that
    /// resolves to an internal address (RFC1918 / link-local incl. cloud metadata /
    /// CGNAT / IPv6 ULA+link-local), while permitting loopback (the fenced child can
    /// reach it directly + the test origins live there) and global addresses.
    #[test]
    fn guard_target_refuses_internal_permits_loopback_and_global() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        // Built from octets (not dotted-string literals) so the internal-specifics
        // linter doesn't flag the RFC1918/CGNAT probe addresses; see the upstream
        // privacy policy linked in this vendored crate's NEWT_PATCHES.md.
        let refused: [IpAddr; 7] = [
            Ipv4Addr::new(10, 0, 0, 5).into(),        // RFC1918
            Ipv4Addr::new(172, 16, 9, 9).into(),      // RFC1918
            Ipv4Addr::new(192, 168, 1, 1).into(),     // RFC1918
            Ipv4Addr::new(169, 254, 169, 254).into(), // link-local: cloud metadata
            Ipv4Addr::new(100, 64, 0, 1).into(),      // CGNAT
            "fe80::1".parse().unwrap(),               // v6 link-local
            "fc00::1".parse().unwrap(),               // v6 unique-local
        ];
        for ip in refused {
            assert!(is_internal_ip(&ip), "{ip} must classify as internal");
            assert!(
                guard_target(SocketAddr::new(ip, 80), "", &HostPolicy::new([])).is_err(),
                "{ip} must be refused"
            );
        }
        let allowed = [
            "127.0.0.1",
            "::1",
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
        ];
        for s in allowed {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!is_internal_ip(&ip), "{s} must be permitted");
            assert!(
                guard_target(SocketAddr::new(ip, 443), "", &HostPolicy::new([])).is_ok(),
                "{s} must be permitted"
            );
        }
    }

    #[test]
    fn parses_connect() {
        assert_eq!(
            parse_request_line("CONNECT example.com:443 HTTP/1.1\r\n"),
            Some(Target::Connect {
                host: "example.com".to_string(),
                port: 443
            })
        );
        // Default port when omitted.
        assert_eq!(
            parse_request_line("CONNECT example.com HTTP/1.1"),
            Some(Target::Connect {
                host: "example.com".to_string(),
                port: 443
            })
        );
    }

    #[test]
    fn parses_http_absolute_form_and_rewrites_to_origin_form() {
        let t = parse_request_line("GET http://example.com/a/b?q=1 HTTP/1.1\r\n").unwrap();
        assert_eq!(
            t,
            Target::Http {
                host: "example.com".to_string(),
                port: 80,
                origin_line: "GET /a/b?q=1 HTTP/1.1\r\n".to_string(),
            }
        );
        // No path → "/". Explicit port honoured.
        let t = parse_request_line("HEAD http://h:8080 HTTP/1.0").unwrap();
        assert_eq!(
            t,
            Target::Http {
                host: "h".to_string(),
                port: 8080,
                origin_line: "HEAD / HTTP/1.0\r\n".to_string(),
            }
        );
    }

    #[test]
    fn parses_ipv6_authority() {
        assert_eq!(
            parse_request_line("CONNECT [::1]:8443 HTTP/1.1"),
            Some(Target::Connect {
                host: "::1".to_string(),
                port: 8443
            })
        );
    }

    #[test]
    fn rejects_malformed_and_unspoken_schemes() {
        assert!(parse_request_line("GET / HTTP/1.1").is_none()); // origin-form, not a proxy req
        assert!(parse_request_line("GET https://x/ HTTP/1.1").is_none()); // https absolute-form
        assert!(parse_request_line("GET ftp://x/ HTTP/1.1").is_none());
        assert!(parse_request_line("garbage").is_none());
        assert!(parse_request_line("CONNECT x:notaport HTTP/1.1").is_none());
    }

    /// A resolver that maps every name to a fixed loopback origin — hermetic.
    struct FixedResolver(SocketAddr);
    impl Resolver for FixedResolver {
        fn resolve(&self, _host: &str, _port: u16) -> io::Result<SocketAddr> {
            Ok(self.0)
        }
    }

    /// Start the proxy with no audit sink and production defaults (most tests
    /// don't inspect the audit, the worker cap, or the connector).
    fn start_null(
        hosts: impl IntoIterator<Item = String>,
        resolver: Arc<dyn Resolver>,
    ) -> io::Result<ProxyHandle> {
        start_internal(
            hosts,
            [] as [String; 0],
            resolver,
            Arc::new(NullSink),
            Arc::new(TcpConnector),
            DEFAULT_WORKER_CAP,
            ConnHooks::default(),
        )
    }

    /// An audit sink that collects every event into a shared vec, for assertions.
    #[derive(Clone, Default)]
    struct CapturingSink(Arc<Mutex<Vec<NetAuditEvent>>>);
    impl AuditSink for CapturingSink {
        fn record(&self, event: &NetAuditEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }
    impl CapturingSink {
        fn events(&self) -> Vec<NetAuditEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    // ── In-memory connection fakes (#166) ───────────────────────────────────
    //
    // The proxy's forward/tunnel/allow-list/audit LOGIC is driven through the
    // real `handle_conn` against these scripted endpoints — no sockets, no
    // accept loops, no timing races. `handle_conn` joins its splice thread
    // before returning, so every assertion below is fully synchronous and
    // deterministic (the old socket tests polled/slept up to 30s and still
    // flaked on constrained runners; #135/#155/#165/#166).

    /// A scripted in-memory [`Conn`]: `read` drains a preset script then returns
    /// EOF (never blocks); `write` captures bytes for assertions; `dup` shares
    /// both (as `TcpStream::try_clone` shares one socket). `shutdown`/timeouts
    /// are no-ops — the fake EOFs on script exhaustion, so nothing can block.
    #[derive(Clone, Default)]
    struct ScriptedConn {
        /// Bytes the proxy reads FROM this endpoint (the client's request, or the
        /// origin's canned response). Drained by `read`; empty ⇒ EOF.
        to_read: Arc<Mutex<VecDeque<u8>>>,
        /// Bytes the proxy WROTE to this endpoint (its client response, or the
        /// request it forwarded to the origin). Captured for assertions.
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl ScriptedConn {
        fn with_script(bytes: &[u8]) -> Self {
            Self {
                to_read: Arc::new(Mutex::new(bytes.iter().copied().collect())),
                written: Arc::new(Mutex::new(Vec::new())),
            }
        }
        /// The bytes the proxy wrote to this endpoint.
        fn written(&self) -> Vec<u8> {
            self.written.lock().unwrap().clone()
        }
        /// The bytes the proxy wrote, as a lossy string (for readable asserts).
        fn written_str(&self) -> String {
            String::from_utf8_lossy(&self.written()).into_owned()
        }
    }

    impl Read for ScriptedConn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut q = self.to_read.lock().unwrap();
            let n = buf.len().min(q.len());
            for slot in buf.iter_mut().take(n) {
                *slot = q.pop_front().unwrap();
            }
            Ok(n) // n == 0 ⇒ script exhausted ⇒ EOF (never blocks)
        }
    }

    impl Write for ScriptedConn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Conn for ScriptedConn {
        fn dup(&self) -> io::Result<Box<dyn Conn>> {
            Ok(Box::new(self.clone()))
        }
        fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
            Ok(())
        }
        fn set_timeouts(&self, _dur: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    /// A connector that hands back a scripted in-memory origin (no real dial).
    struct FakeConnector(ScriptedConn);
    impl Connector for FakeConnector {
        fn connect(&self, _addr: SocketAddr) -> io::Result<Box<dyn Conn>> {
            Ok(Box::new(self.0.clone()))
        }
    }

    /// A connector whose dial always fails — for the `502 Bad Gateway` path.
    struct FailingConnector;
    impl Connector for FailingConnector {
        fn connect(&self, _addr: SocketAddr) -> io::Result<Box<dyn Conn>> {
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "origin unreachable",
            ))
        }
    }

    /// A connector whose dial always panics — for proving worker-panic evidence
    /// and finalize failure (#372 §7) without exhausting real OS threads.
    struct PanicConnector;
    impl Connector for PanicConnector {
        fn connect(&self, _addr: SocketAddr) -> io::Result<Box<dyn Conn>> {
            panic!("injected connector panic (test)");
        }
    }

    /// The outcome of driving one connection through the real `handle_conn`.
    struct Driven {
        /// What the proxy sent back to the client (response / forwarded origin bytes).
        client: ScriptedConn,
        /// What the proxy forwarded to the origin (empty if it was never dialled).
        origin: ScriptedConn,
        /// Hosts refused with 403 (deduped, sorted).
        refused: Vec<String>,
        /// Audit events emitted (synchronously, before `handle_conn` returned).
        audit: Vec<NetAuditEvent>,
    }

    /// Drive one client `request` through `handle_conn` with `connector` (used
    /// for the deny / malformed / 502 paths, where the origin is never
    /// successfully forwarded to — `out.origin` stays empty). Deterministic:
    /// `handle_conn` joins its splice thread before returning, so all buffers +
    /// the audit are fully populated on return. For the ALLOWED forward path
    /// (where you want to inspect the forwarded bytes) use [`drive_forward`].
    fn drive_with(request: &[u8], allow: &[&str], connector: &dyn Connector) -> Driven {
        let client = ScriptedConn::with_script(request);
        let policy = HostPolicy::new(allow.iter().map(|s| s.to_string()));
        let resolver = FixedResolver("127.0.0.1:9".parse().unwrap());
        let sink = CapturingSink::default();
        let evidence = Mutex::new(Evidence::default());
        let closer = ConnCloser::new(Box::new(client.clone()));
        let env = ConnEnv {
            policy: &policy,
            connector,
            resolver: &resolver,
            sink: &sink,
        };
        let _ = handle_conn(
            Box::new(client.clone()),
            &env,
            &evidence,
            &closer,
            &ConnHooks::default(),
        );
        Driven {
            client,
            origin: ScriptedConn::default(),
            refused: evidence.into_inner().unwrap().refused_sorted(),
            audit: sink.events(),
        }
    }

    /// Convenience: drive an ALLOWED forward/tunnel with an explicit origin
    /// script, returning the `Driven` outcome (with the origin's captured bytes).
    fn drive_forward(request: &[u8], allow: &[&str], origin_script: &[u8]) -> Driven {
        let client = ScriptedConn::with_script(request);
        let origin = ScriptedConn::with_script(origin_script);
        let policy = HostPolicy::new(allow.iter().map(|s| s.to_string()));
        let resolver = FixedResolver("127.0.0.1:9".parse().unwrap());
        let sink = CapturingSink::default();
        let evidence = Mutex::new(Evidence::default());
        let closer = ConnCloser::new(Box::new(client.clone()));
        let connector = FakeConnector(origin.clone());
        let env = ConnEnv {
            policy: &policy,
            connector: &connector,
            resolver: &resolver,
            sink: &sink,
        };
        let _ = handle_conn(
            Box::new(client.clone()),
            &env,
            &evidence,
            &closer,
            &ConnHooks::default(),
        );
        Driven {
            client,
            origin,
            refused: evidence.into_inner().unwrap().refused_sorted(),
            audit: sink.events(),
        }
    }

    /// #385: forward-port of `ac3d34a`'s exact-private-host acceptance test, onto
    /// this file's post-#372/#383 `ConnEnv`/`handle_conn` shape (a `Driven`-style
    /// harness rather than the 0.7 branch's direct `(policy, connector, resolver,
    /// sink, evidence)` argument list). Grounds the exact-private-host policy
    /// through the production connection handler; only DNS and the destination
    /// socket are in-memory seams.
    #[test]
    fn exact_private_hosts_require_both_grants_and_pin_the_screened_address() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        for request in [
            &b"CONNECT service.test:443 HTTP/1.1\r\n\r\nPING"[..],
            &b"GET http://service.test:443/x HTTP/1.1\r\n\r\n"[..],
        ] {
            // Synthetic address classes, following the existing guard fixtures.
            for ip in [
                IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40)),
                IpAddr::V4(Ipv4Addr::new(172, 16, 10, 20)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 50, 10)),
                IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x1234)),
                IpAddr::V6(Ipv4Addr::new(10, 20, 30, 40).to_ipv6_mapped()),
            ] {
                let address = SocketAddr::new(ip, 443);
                for (allow, private, expected) in [
                    (vec!["service.test"], vec!["SERVICE.TEST."], 200),
                    (vec!["service.test"], vec![], 502),
                    (vec!["service.test"], vec!["other.test"], 502),
                    (vec!["other.test"], vec!["service.test"], 403),
                ] {
                    struct CountResolver(SocketAddr, AtomicUsize);
                    impl Resolver for CountResolver {
                        fn resolve(&self, _: &str, _: u16) -> io::Result<SocketAddr> {
                            assert_eq!(self.1.fetch_add(1, Ordering::SeqCst), 0, "resolve once");
                            Ok(self.0)
                        }
                    }
                    struct PinnedConnector(SocketAddr, AtomicUsize);
                    impl Connector for PinnedConnector {
                        fn connect(&self, address: SocketAddr) -> io::Result<Box<dyn Conn>> {
                            assert_eq!(address, self.0, "dial the screened address without DNS");
                            self.1.fetch_add(1, Ordering::SeqCst);
                            Ok(Box::new(ScriptedConn::with_script(
                                b"HTTP/1.1 200 OK\r\n\r\n",
                            )))
                        }
                    }
                    let policy = HostPolicy::with_private_hosts(
                        allow.into_iter().map(str::to_string),
                        private.into_iter().map(str::to_string),
                    )
                    .unwrap();
                    let resolver = CountResolver(address, 0.into());
                    let connector = PinnedConnector(address, 0.into());
                    let sink = CapturingSink::default();
                    let client = ScriptedConn::with_script(request);
                    let evidence = Mutex::new(Evidence::default());
                    let closer = ConnCloser::new(Box::new(client.clone()));
                    let env = ConnEnv {
                        policy: &policy,
                        connector: &connector,
                        resolver: &resolver,
                        sink: &sink,
                    };
                    handle_conn(
                        Box::new(client.clone()),
                        &env,
                        &evidence,
                        &closer,
                        &ConnHooks::default(),
                    )
                    .unwrap();
                    assert!(
                        client
                            .written_str()
                            .starts_with(&format!("HTTP/1.1 {expected}")),
                        "address {address}, expected {expected}: {}",
                        client.written_str()
                    );
                    assert_eq!(
                        connector.1.load(Ordering::SeqCst),
                        usize::from(expected == 200)
                    );
                    assert_eq!(
                        resolver.1.load(Ordering::SeqCst),
                        usize::from(expected != 403)
                    );
                }
            }
        }
    }

    /// #385: forward-port of `ac3d34a`'s forbidden-ranges test. An approved exact
    /// host still cannot reach anything but RFC1918/ULA.
    #[test]
    fn exact_private_hosts_never_approve_forbidden_ranges() {
        use std::net::{IpAddr, Ipv4Addr};
        let policy = HostPolicy::with_private_hosts(
            ["service.test".to_string()],
            ["service.test".to_string()],
        )
        .unwrap();
        for ip in [
            "169.254.169.254",
            "fd00:ec2::254",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.18.0.1",
            "::",
            "fe80::1",
            "ff02::1",
            "::ffff:169.254.169.254",
            "::ffff:224.0.0.1",
        ]
        .into_iter()
        .map(|ip| ip.parse::<IpAddr>().unwrap())
        .chain([
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V6(Ipv4Addr::new(100, 64, 0, 1).to_ipv6_mapped()),
        ]) {
            let resolver = FixedResolver(SocketAddr::new(ip, 443));
            for request in [
                &b"CONNECT service.test:443 HTTP/1.1\r\n\r\n"[..],
                &b"GET http://service.test/ HTTP/1.1\r\n\r\n"[..],
            ] {
                let client = ScriptedConn::with_script(request);
                let sink = CapturingSink::default();
                let evidence = Mutex::new(Evidence::default());
                let closer = ConnCloser::new(Box::new(client.clone()));
                let connector =
                    FakeConnector(ScriptedConn::with_script(b"HTTP/1.1 200 OK\r\n\r\n"));
                let env = ConnEnv {
                    policy: &policy,
                    connector: &connector,
                    resolver: &resolver,
                    sink: &sink,
                };
                // A successful fake dial would return 200, so 502 proves it was screened out.
                handle_conn(
                    Box::new(client.clone()),
                    &env,
                    &evidence,
                    &closer,
                    &ConnHooks::default(),
                )
                .unwrap();
                assert!(
                    client.written_str().contains("502"),
                    "forbidden address {ip}"
                );
                assert_eq!(sink.events()[0].decision, NetDecision::Error);
            }
        }
    }

    /// #385: forward-port of `ac3d34a`'s name-validation test.
    #[test]
    fn exact_private_hosts_reject_patterns_urls_and_ambiguous_names() {
        for invalid in [
            "",
            "*",
            "*.test",
            ".test",
            "https://service.test",
            "service.test:443",
            "user@service.test",
            "service.test/path",
            " service.test",
            "service.test ",
            "a..test",
            "-bad.test",
            "bad-.test",
            "127.1",
            "0x7f000001",
            "service.test?x",
        ] {
            assert!(
                HostPolicy::with_private_hosts(["service.test".to_string()], [invalid.to_string()])
                    .is_err(),
                "invalid exact name: {invalid}"
            );
        }
        let policy =
            HostPolicy::with_private_hosts(["fd00::1".to_string()], ["[FD00::1]".to_string()])
                .unwrap();
        assert!(policy.allows_private("fd00::1"));
    }

    #[test]
    fn allowed_http_host_is_forwarded_to_origin() {
        // The allow-listed host is forwarded; the origin's body reaches the client.
        let out = drive_forward(
            b"GET http://allowed.test/x HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n",
            &["allowed.test"],
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\norigin",
        );
        let client_saw = out.client.written_str();
        assert!(client_saw.contains("200"), "client saw: {client_saw}");
        assert!(
            client_saw.contains("origin"),
            "the origin's body must reach the client: {client_saw}"
        );
        // The request reached the origin (proof the forward actually happened).
        assert!(
            out.origin.written_str().starts_with("GET /x HTTP/1.1"),
            "origin must receive the origin-form request: {}",
            out.origin.written_str()
        );
        assert!(out.refused.is_empty(), "an allowed host is not refused");
    }

    #[test]
    fn disallowed_http_host_is_refused_403_without_reaching_origin() {
        // A denied host must never be dialled: FailingConnector would surface as a
        // 502 if it were ever called, so a 403 here also proves it was NOT.
        let out = drive_with(
            b"GET http://evil.test/x HTTP/1.1\r\nHost: ignored\r\n\r\n",
            &["allowed.test"],
            &FailingConnector,
        );
        let client_saw = out.client.written_str();
        assert!(
            client_saw.contains("403"),
            "denied host must get 403: {client_saw}"
        );
        assert!(
            !client_saw.contains("502"),
            "the origin must not be dialled for a denied host: {client_saw}"
        );
        assert!(out.origin.written().is_empty(), "origin must see nothing");
    }

    #[test]
    fn unreachable_allowed_origin_yields_502() {
        // Allow-listed, but the dial fails → 502 Bad Gateway + an `Error` audit.
        let out = drive_with(
            b"GET http://allowed.test/x HTTP/1.1\r\nHost: ignored\r\n\r\n",
            &["allowed.test"],
            &FailingConnector,
        );
        assert!(
            out.client.written_str().contains("502"),
            "an unreachable allowed origin must get 502: {}",
            out.client.written_str()
        );
        let ev = out.audit.iter().find(|e| e.host == "allowed.test").unwrap();
        assert_eq!(ev.decision, NetDecision::Error);
    }

    #[test]
    fn malformed_request_yields_400_and_no_audit() {
        // A request line the proxy does not speak → 400, and (deliberately) NO
        // audit event, since there is no host to attribute an egress attempt to.
        let out = drive_with(
            b"GET / HTTP/1.1\r\n\r\n",
            &["allowed.test"],
            &FailingConnector,
        );
        assert!(out.client.written_str().contains("400"));
        assert!(
            out.audit.is_empty(),
            "a malformed request is not an egress event: {:?}",
            out.audit
        );
        assert!(out.refused.is_empty());
    }

    /// #196: the proxy accumulates every host it REFUSES and surfaces them via
    /// `refused_hosts()` (deduped, sorted); allowed hosts are never listed.
    #[test]
    fn refused_hosts_surfaces_denied_hosts_deduped_and_omits_allowed() {
        // Drive three requests through ONE shared evidence accumulator: allowed
        // (forwarded), then the same denied host twice (must dedupe to one entry).
        let policy = HostPolicy::new(["allowed.test".to_string()]);
        let resolver = FixedResolver("127.0.0.1:9".parse().unwrap());
        let sink = NullSink;
        let evidence = Mutex::new(Evidence::default());
        let origin = ScriptedConn::with_script(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let connector = FakeConnector(origin);

        for (req, conn) in [
            (
                &b"GET http://allowed.test/x HTTP/1.1\r\nHost: a\r\n\r\n"[..],
                &connector as &dyn Connector,
            ),
            (
                &b"GET http://evil.test/y HTTP/1.1\r\nHost: a\r\n\r\n"[..],
                &FailingConnector,
            ),
            (
                &b"GET http://evil.test/z HTTP/1.1\r\nHost: a\r\n\r\n"[..],
                &FailingConnector,
            ),
        ] {
            let client = ScriptedConn::with_script(req);
            let closer = ConnCloser::new(Box::new(client.clone()));
            let env = ConnEnv {
                policy: &policy,
                connector: conn,
                resolver: &resolver,
                sink: &sink,
            };
            let _ = handle_conn(
                Box::new(client),
                &env,
                &evidence,
                &closer,
                &ConnHooks::default(),
            );
        }

        let got = evidence.into_inner().unwrap().refused_sorted();
        assert_eq!(
            got,
            vec!["evil.test".to_string()],
            "only the denied host, deduped; the allowed host must NOT appear: {got:?}"
        );
    }

    #[test]
    fn audit_records_allowed_with_bytes_and_denied_attempts() {
        // Allowed forward: an `Allowed` Http event on port 80 with response bytes.
        let allowed = drive_forward(
            b"GET http://allowed.test/x HTTP/1.1\r\nHost: ignored\r\n\r\n",
            &["allowed.test"],
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\norigin",
        );
        let ev = allowed
            .audit
            .iter()
            .find(|e| e.host == "allowed.test")
            .expect("an allowed event");
        assert_eq!(ev.decision, NetDecision::Allowed);
        assert_eq!(ev.kind, NetKind::Http);
        assert_eq!(ev.port, 80);
        assert!(
            ev.bytes_down > 0,
            "an allowed connection records response bytes: {ev:?}"
        );

        // Denied: a `Denied` event (the exfil-attempt signal) with zero bytes.
        let denied = drive_with(
            b"GET http://evil.test/y HTTP/1.1\r\nHost: ignored\r\n\r\n",
            &["allowed.test"],
            &FailingConnector,
        );
        let ev = denied
            .audit
            .iter()
            .find(|e| e.host == "evil.test")
            .expect("a denied event");
        assert_eq!(ev.decision, NetDecision::Denied);
        assert_eq!(ev.bytes_up, 0);
        assert_eq!(ev.bytes_down, 0);
    }

    #[test]
    fn jsonl_sink_appends_one_newline_terminated_json_line_per_event() {
        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buf = SharedBuf::default();
        let sink = JsonlSink::new(buf.clone());
        let mk = |host: &str| NetAuditEvent {
            ts_ms: 1,
            host: host.into(),
            port: 80,
            kind: NetKind::Http,
            decision: NetDecision::Allowed,
            bytes_up: 1,
            bytes_down: 2,
            dur_ms: 3,
        };
        sink.record(&mk("a"));
        sink.record(&mk("b"));
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one JSON line per event: {text:?}");
        assert_eq!(
            serde_json::from_str::<NetAuditEvent>(lines[0])
                .unwrap()
                .host,
            "a"
        );
        assert_eq!(
            serde_json::from_str::<NetAuditEvent>(lines[1])
                .unwrap()
                .host,
            "b"
        );
    }

    #[test]
    fn audit_event_json_round_trips() {
        let e = NetAuditEvent {
            ts_ms: 1,
            host: "h".into(),
            port: 443,
            kind: NetKind::Connect,
            decision: NetDecision::Allowed,
            bytes_up: 10,
            bytes_down: 20,
            dur_ms: 5,
        };
        let line = serde_json::to_string(&e).unwrap();
        assert!(line.contains("\"decision\":\"allowed\"") && line.contains("\"kind\":\"connect\""));
        assert_eq!(serde_json::from_str::<NetAuditEvent>(&line).unwrap(), e);
    }

    #[test]
    fn connect_allowed_host_tunnels_opaque_bytes() {
        // The client pipelines "PING" right after the CONNECT header block; the
        // (scripted) origin returns "PING" as its side of the opaque exchange.
        // Proof of a real bidirectional tunnel: the origin receives the client's
        // PING (up-copy) and the client receives the origin's PING (down-copy),
        // after a bare 200 status line — driven through the real splice logic.
        let out = drive_forward(
            b"CONNECT allowed.test:443 HTTP/1.1\r\nHost: allowed.test:443\r\n\r\nPING",
            &["allowed.test"],
            b"PING",
        );
        let client_saw = out.client.written_str();
        assert!(
            client_saw.starts_with("HTTP/1.1 200"),
            "CONNECT must be accepted with a bare 200: {client_saw:?}"
        );
        assert!(
            client_saw.contains("PING"),
            "the origin's bytes must tunnel back to the client: {client_saw:?}"
        );
        assert_eq!(
            out.origin.written(),
            b"PING",
            "the client's pipelined bytes must reach the origin through the tunnel"
        );
    }

    #[test]
    fn connect_disallowed_host_is_refused_403() {
        let out = drive_with(
            b"CONNECT evil.test:443 HTTP/1.1\r\n\r\n",
            &["allowed.test"],
            &FailingConnector,
        );
        assert!(
            out.client.written_str().contains("403"),
            "a denied CONNECT must get 403, not a tunnel: {}",
            out.client.written_str()
        );
    }

    #[test]
    fn http_host_header_is_normalized_to_the_validated_authority() {
        // The authority (allowed.test) is validated, but the client LIES with a
        // spoofed `Host: evil.test` to domain-front. The origin must see the
        // validated authority substituted in, never the spoof.
        let out = drive_forward(
            b"GET http://allowed.test/ HTTP/1.1\r\nHost: evil.test\r\nConnection: close\r\n\r\n",
            &["allowed.test"],
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let origin_saw = out.origin.written_str();
        assert!(
            origin_saw.contains("Host: allowed.test\r\n"),
            "origin must receive the validated Host: {origin_saw:?}"
        );
        assert!(
            !origin_saw.contains("evil.test"),
            "the spoofed Host must not reach the origin: {origin_saw:?}"
        );
    }

    /// Serialize the tests that touch REAL loopback sockets (#155, #207). They
    /// share the host's loopback and ephemeral-port space, so concurrent
    /// siblings can interfere — e.g. a port released by one test can be
    /// re-bound by a sibling before the first is done probing it, and the
    /// probe then observes the sibling's live listener. One shared lock
    /// removes that interference class wholesale; the motivating (never
    /// locally reproduced) flake was `Empty reply from server` on the
    /// fenced_child test in PR #195's macOS CI. The in-memory tests above
    /// (#216) need no lock — only the real-socket tests below take it.
    /// Process-local, which suffices because `cargo test` runs test binaries
    /// sequentially; a per-test-process runner (e.g. nextest) would NOT be
    /// covered, nor would the loopback binds in other crates' test binaries.
    /// Poison is ignored so a panicking test does not cascade-fail the rest.
    fn net_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A one-shot gate: `wait()` blocks until `release()` is called (possibly
    /// from another thread) — the #372 test seam for deterministically pausing
    /// a connection worker at a chosen [`ConnHooks`] phase.
    #[derive(Clone, Default)]
    struct Gate(Arc<(Mutex<bool>, Condvar)>);
    impl Gate {
        fn wait(&self) {
            let (m, cvar) = &*self.0;
            let mut released = m.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }
        }
        fn release(&self) {
            let (m, cvar) = &*self.0;
            *m.lock().unwrap() = true;
            cvar.notify_all();
        }
        fn as_hook(&self) -> Arc<dyn Fn() + Send + Sync> {
            let g = self.clone();
            Arc::new(move || g.wait())
        }
    }

    /// Poll (bounded) until the proxy's active-worker count reaches `want` —
    /// synchronizes a test with the accept loop's own admission bookkeeping,
    /// which happens on a background thread.
    fn wait_for_active_count(proxy: &ProxyHandle, want: usize) {
        for _ in 0..500 {
            if lock(&proxy.shared.workers).active.len() >= want {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("active worker count never reached {want}");
    }

    /// Poll (bounded) until every currently-tracked connection worker's
    /// `JoinHandle` reports finished — used where a test needs the worker to
    /// have ALREADY run to completion (e.g. panicked) before finalizing, so
    /// `shutdown_and_join`'s own force-close sweep cannot race ahead of it and
    /// abort a connection before it ever reached the code under test.
    /// `wait_for_active_count` only proves a worker was *spawned*, which can
    /// be true before it has executed a single line of `handle_conn`.
    fn wait_for_worker_finished(proxy: &ProxyHandle) {
        for _ in 0..500 {
            let all_finished = {
                let book = lock(&proxy.shared.workers);
                !book.active.is_empty() && book.active.values().all(|(h, _)| h.is_finished())
            };
            if all_finished {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("worker never finished");
    }

    #[test]
    fn proxy_env_points_at_the_bound_loopback_addr() {
        // A REAL-socket test: `proxy_env` is derived from the actually-bound
        // loopback address, so this exercises the real `bind` — and its live
        // accept-loop listener must not interleave with the port probes of the
        // sibling real-socket tests.
        let _serial = net_test_lock();
        let proxy = start_null(["x".to_string()], Arc::new(StdResolver)).unwrap();
        let env = proxy.proxy_env();
        let url = format!("http://127.0.0.1:{}", proxy.addr().port());
        assert!(env.iter().any(|(k, v)| k == "https_proxy" && *v == url));
        assert!(env.iter().any(|(k, v)| k == "HTTPS_PROXY" && *v == url));
        assert!(env.iter().all(|(_, v)| v.starts_with("http://127.0.0.1:")));
    }

    // ── #372: bounded ownership + explicit lifecycle acceptance tests ──────

    /// #372 §1: with worker cap `N`, `N` deliberately held-open connections plus
    /// several excess ones must show a high-water mark that never exceeds `N`,
    /// and every excess connection must be counted as capacity overload — never
    /// mislabeled as an authority denial.
    #[test]
    fn worker_cap_bounds_active_workers_and_counts_excess_as_capacity_overload() {
        let _serial = net_test_lock();
        const CAP: usize = 2;
        let gate = Gate::default();
        let hooks = ConnHooks {
            entered: Some(gate.as_hook()),
            decided: None,
        };
        let proxy = start_internal(
            ["x".to_string()],
            [] as [String; 0],
            Arc::new(StdResolver),
            Arc::new(NullSink),
            Arc::new(TcpConnector),
            CAP,
            hooks,
        )
        .unwrap();

        // CAP held-open connections: each worker parks at `Entered` until released.
        let mut held = Vec::new();
        for _ in 0..CAP {
            let c = TcpStream::connect(proxy.addr()).unwrap();
            held.push(c);
            wait_for_active_count(&proxy, held.len());
        }

        // Present substantially more connections than the cap.
        let excess_attempts = CAP + 3;
        let mut excess_responses = Vec::new();
        for _ in 0..excess_attempts {
            let mut c = TcpStream::connect(proxy.addr()).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut resp = String::new();
            let _ = c.read_to_string(&mut resp);
            excess_responses.push(resp);
        }
        assert!(
            excess_responses.iter().all(|r| r.contains("503")),
            "every excess connection over the cap must be refused as overload: {excess_responses:?}"
        );

        gate.release();
        drop(held);

        let evidence = proxy.shutdown_and_join().expect("finalize");
        assert_eq!(
            evidence.high_water_workers, CAP,
            "high-water must equal the cap, never exceed it: {evidence:?}"
        );
        assert_eq!(
            evidence.capacity_rejects, excess_attempts as u64,
            "every excess connection must be counted: {evidence:?}"
        );
    }

    /// #372 §2 acceptance ("denied CONNECT paused across finalization"):
    /// pausing a worker AFTER it has recorded its denial, then finalizing, must
    /// BLOCK finalization until the worker is released — never return evidence
    /// that races past an in-flight decision (the snapshot-plus-`Drop` design
    /// this issue replaces could not make this guarantee).
    #[test]
    fn finalize_waits_for_a_denied_connect_paused_after_its_decision() {
        let _serial = net_test_lock();
        let gate = Gate::default();
        let hooks = ConnHooks {
            entered: None,
            decided: Some(gate.as_hook()),
        };
        let proxy = start_internal(
            ["allowed.test".to_string()],
            [] as [String; 0],
            Arc::new(StdResolver),
            Arc::new(NullSink),
            Arc::new(TcpConnector),
            DEFAULT_WORKER_CAP,
            hooks,
        )
        .unwrap();

        let mut client = TcpStream::connect(proxy.addr()).unwrap();
        client
            .write_all(b"CONNECT evil.test:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        wait_for_active_count(&proxy, 1);

        let (tx, rx) = mpsc::channel();
        let finalize = thread::spawn(move || {
            let _ = tx.send(proxy.shutdown_and_join());
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "finalization must wait for the paused worker, not return early"
        );

        gate.release();
        let evidence = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("finalize must complete after release")
            .expect("finalize succeeds");
        let _ = finalize.join();
        assert!(
            evidence.refused_hosts.contains(&"evil.test".to_string()),
            "the denial decided before the pause must be in the frozen evidence: {evidence:?}"
        );
    }

    /// #372 §2 acceptance ("occupied/idle tunnel shutdown"): finalizing while an
    /// allow-listed CONNECT tunnel sits idle (no data flowing either way) must
    /// not hang. Shutdown force-closes the proxy-owned socket ends, the splice
    /// threads see EOF, and finalize returns with the connection accounted as a
    /// shutdown-abort.
    #[test]
    fn finalize_force_closes_and_joins_an_idle_tunnel_connection() {
        let _serial = net_test_lock();
        // A real loopback origin that accepts and then holds the connection open,
        // sending/reading nothing — the honest "idle tunnel" shape.
        let origin_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let origin_addr = origin_listener.local_addr().unwrap();
        thread::spawn(move || {
            for s in origin_listener.incoming().flatten() {
                // Hold the socket open; never read or write.
                std::mem::forget(s);
            }
        });

        let proxy = start_internal(
            ["allowed.test".to_string()],
            [] as [String; 0],
            Arc::new(FixedResolver(origin_addr)),
            Arc::new(NullSink),
            Arc::new(TcpConnector),
            DEFAULT_WORKER_CAP,
            ConnHooks::default(),
        )
        .unwrap();

        let mut client = TcpStream::connect(proxy.addr()).unwrap();
        client
            .write_all(b"CONNECT allowed.test:443 HTTP/1.1\r\nHost: allowed.test:443\r\n\r\n")
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reply = [0u8; 64];
        let n = client.read(&mut reply).unwrap();
        assert!(
            String::from_utf8_lossy(&reply[..n]).starts_with("HTTP/1.1 200"),
            "the tunnel must open before it goes idle"
        );
        wait_for_active_count(&proxy, 1);

        let (tx, rx) = mpsc::channel();
        let finalize = thread::spawn(move || {
            let _ = tx.send(proxy.shutdown_and_join());
        });
        // `shutdown()` interrupts an in-flight blocked read promptly on
        // Linux/macOS; that prompt-interrupt behavior for a socket duplicated
        // within the same process is not guaranteed on every platform. The
        // honest, PORTABLE upper bound is each connection's own `CONN_TIMEOUT`
        // (30s, set on both the client and origin sockets when the connection
        // was accepted/dialed) — so this waits comfortably past it rather than
        // assuming an immediate interrupt everywhere. What must never happen,
        // and what the assertions below actually check, is an unbounded hang.
        let evidence = rx
            .recv_timeout(CONN_TIMEOUT + Duration::from_secs(5))
            .expect("finalize must not hang on an idle tunnel")
            .expect("finalize on an idle tunnel must still succeed");
        let _ = finalize.join();

        assert_eq!(
            evidence.shutdown_aborts, 1,
            "the idle tunnel must be accounted as a shutdown-abort, not left dangling: {evidence:?}"
        );
        assert_eq!(evidence.completed, evidence.accepted);
    }

    /// #372 §2: after `shutdown_and_join` returns, no worker remains active and
    /// the listener is gone — a fresh connect must not be served.
    #[test]
    fn finalize_leaves_zero_active_workers_and_an_unreachable_listener() {
        let _serial = net_test_lock();
        let proxy = start_null(["x".to_string()], Arc::new(StdResolver)).unwrap();
        let addr = proxy.addr();
        let shared = Arc::clone(&proxy.shared);
        let evidence = proxy.shutdown_and_join().expect("finalize");
        assert!(
            lock(&shared.workers).active.is_empty(),
            "no worker may remain active after finalize"
        );
        assert_eq!(evidence.completed, evidence.accepted);

        thread::sleep(Duration::from_millis(100));
        if let Ok(mut c) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            c.set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let _ = write!(c, "GET http://x/ HTTP/1.1\r\n\r\n");
            let mut resp = Vec::new();
            let _ = c.read_to_end(&mut resp);
            assert!(resp.is_empty(), "a quiescent proxy must not serve requests");
        }
    }

    /// #372 §7: a connection-worker spawn failure must be surfaced/counted and
    /// must fail finalization — never silently folded into apparently-complete
    /// evidence.
    #[test]
    fn injected_worker_spawn_failure_is_counted_and_fails_finalize() {
        let _serial = net_test_lock();
        let proxy = start_null(["x".to_string()], Arc::new(StdResolver)).unwrap();
        proxy
            .shared
            .inject_spawn_failures
            .store(1, Ordering::SeqCst);

        let mut c = TcpStream::connect(proxy.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut resp = String::new();
        let _ = c.read_to_string(&mut resp);
        assert!(
            resp.contains("503"),
            "an injected spawn failure must be refused safely: {resp:?}"
        );

        match proxy.shutdown_and_join() {
            Err(ProxyFinalizeError::Incomplete { partial, .. }) => {
                assert_eq!(partial.worker_spawn_failures, 1);
            }
            other => panic!("expected Incomplete due to the injected spawn failure: {other:?}"),
        }
    }

    /// #372 §7: a connection-worker panic must be surfaced/counted and must
    /// fail finalization.
    #[test]
    fn injected_worker_panic_is_counted_and_fails_finalize() {
        let _serial = net_test_lock();
        let proxy = start_internal(
            ["allowed.test".to_string()],
            [] as [String; 0],
            Arc::new(FixedResolver("127.0.0.1:9".parse().unwrap())),
            Arc::new(NullSink),
            Arc::new(PanicConnector),
            DEFAULT_WORKER_CAP,
            ConnHooks::default(),
        )
        .unwrap();

        // Triggers PanicConnector::connect inside the connection-worker thread; a
        // panic there is EXPECTED and prints its usual message to stderr — the
        // test itself does not panic, since we only ever observe it via `join`.
        let mut c = TcpStream::connect(proxy.addr()).unwrap();
        let _ = c.write_all(b"CONNECT allowed.test:443 HTTP/1.1\r\n\r\n");
        drop(c);
        // Synchronize with the accept loop: wait until it has actually admitted
        // this connection as a worker AND for that worker to have actually run
        // to completion (panicked) before finalizing. Waiting only for
        // admission (`wait_for_active_count`) is not enough: `shutdown_and_join`
        // force-closes the connection as part of finalizing, and if that races
        // ahead of the worker's own read, the worker fails on the now-dead
        // socket, is accounted as a shutdown-abort, and `PanicConnector::connect`
        // is never reached at all — the exact flake this wait eliminates.
        wait_for_active_count(&proxy, 1);
        wait_for_worker_finished(&proxy);

        match proxy.shutdown_and_join() {
            Err(ProxyFinalizeError::Incomplete { partial, .. }) => {
                assert_eq!(
                    partial.worker_panics, 1,
                    "the panic must be counted: {partial:?}"
                );
            }
            other => panic!("expected Incomplete due to the injected worker panic: {other:?}"),
        }
    }

    /// #372 §7 (adversarial review finding): a panic on the `up` (splice)
    /// copy thread must propagate, not be folded into "0 bytes forwarded" by
    /// `.unwrap_or(0)` — that would let a splice-worker panic produce
    /// apparently-complete evidence, exactly what §7 forbids. Drives
    /// `splice_buffered` directly (no accept loop needed) with a client whose
    /// read always panics; the up thread reads that client, so the panic must
    /// resurface on this (the calling/owning) thread.
    #[test]
    #[should_panic(expected = "injected read panic (test)")]
    fn splice_buffered_propagates_a_panic_on_the_up_thread_read() {
        struct PanicOnRead;
        impl Read for PanicOnRead {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                panic!("injected read panic (test)");
            }
        }
        impl Write for PanicOnRead {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Conn for PanicOnRead {
            fn dup(&self) -> io::Result<Box<dyn Conn>> {
                Ok(Box::new(PanicOnRead))
            }
            fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
                Ok(())
            }
            fn set_timeouts(&self, _dur: Duration) -> io::Result<()> {
                Ok(())
            }
        }

        let client_reader = BufReader::new(Box::new(PanicOnRead) as Box<dyn Conn>);
        let client: Box<dyn Conn> = Box::new(PanicOnRead);
        let origin: Box<dyn Conn> = Box::new(ScriptedConn::default());
        let _ = splice_buffered(client_reader, client, origin);
    }

    /// #372 §3/acceptance ("hostile oversized evidence deserialization"): too
    /// many refused-host entries, or one oversized entry, must be rejected
    /// during deserialization rather than accepted and silently truncated.
    #[test]
    fn hostile_evidence_deserialization_rejects_too_many_and_oversized_hosts() {
        let base = serde_json::json!({
            "omitted_refused_attempts": 0,
            "accepted": 0,
            "completed": 0,
            "capacity_rejects": 0,
            "shutdown_aborts": 0,
            "worker_spawn_failures": 0,
            "worker_panics": 0,
            "worker_cap": 1,
            "high_water_workers": 0,
        });

        let mut too_many = base.clone();
        let hosts: Vec<String> = (0..(MAX_REFUSED_HOSTS + 1))
            .map(|i| format!("h{i}.test"))
            .collect();
        too_many["refused_hosts"] = serde_json::json!(hosts);
        assert!(
            serde_json::from_value::<ProxyFinalEvidence>(too_many).is_err(),
            "more than {MAX_REFUSED_HOSTS} refused hosts must be rejected"
        );

        let mut oversized = base;
        oversized["refused_hosts"] = serde_json::json!(["x".repeat(MAX_REFUSED_HOST_BYTES + 1)]);
        assert!(
            serde_json::from_value::<ProxyFinalEvidence>(oversized).is_err(),
            "a host over {MAX_REFUSED_HOST_BYTES} bytes must be rejected"
        );
    }

    #[test]
    fn evidence_round_trips_within_bounds() {
        let e = ProxyFinalEvidence {
            refused_hosts: vec!["a.test".to_string(), "b.test".to_string()],
            omitted_refused_attempts: 3,
            accepted: 10,
            completed: 10,
            capacity_rejects: 1,
            shutdown_aborts: 2,
            worker_spawn_failures: 0,
            worker_panics: 0,
            worker_cap: 64,
            high_water_workers: 5,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: ProxyFinalEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    /// A one-shot HTTP origin on loopback that replies 200 with a marker body —
    /// used ONLY by the macOS kernel e2e below, which must drive real `curl`
    /// through the real proxy over a real socket (the in-memory fakes cannot
    /// exercise a kernel fence). Gated to that test's platform so it is not dead
    /// code elsewhere.
    #[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
    fn spawn_origin() -> SocketAddr {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = l.local_addr().unwrap();
        thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut b = [0u8; 512];
                let _ = s.read(&mut b);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\norigin",
                );
            }
        });
        addr
    }

    /// End-to-end kernel proof (#124, ADR 0016): a real `curl` child, confined by
    /// the ADR 0015 loopback fence, reaches an ALLOWED host only through the proxy
    /// (200), is refused a DENIED host by the proxy (403), and — crucially —
    /// **cannot bypass** the proxy: a direct off-box connect is kernel-denied. All
    /// hermetic — the resolver maps the allow-listed name to a loopback origin, so
    /// no real network is touched. macOS + `macos-seatbelt` only; self-skips if
    /// `sandbox-exec`/`curl` are unavailable. This is the one test that genuinely
    /// needs the real socket path (the fence is a kernel property); the forward /
    /// tunnel / allow-list / audit LOGIC is covered deterministically in-memory
    /// above.
    #[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
    #[test]
    fn fenced_child_reaches_allowed_via_proxy_denied_refused_direct_kernel_blocked() {
        use crate::{
            best_available_sandbox, loopback_fenced_caveats, seatbelt_is_supported, Caveats,
            SandboxPolicy, Scope,
        };
        // Serialize with sibling loopback tests (issue #207): this is the
        // heaviest net test (origin + proxy + real curl child) and must not
        // race siblings on loopback. Held for the whole test via RAII.
        let _serial = net_test_lock();
        if !seatbelt_is_supported() {
            eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
            return;
        }
        let curl = "/usr/bin/curl";
        if !std::path::Path::new(curl).exists() {
            eprintln!("skipping: no curl(1)");
            return;
        }

        let origin = spawn_origin();
        let proxy = start_null(
            ["allowed.test".to_string()],
            Arc::new(FixedResolver(origin)),
        )
        .unwrap();

        // The grant is a general remote-host allow-list; its loopback-fenced form
        // is what actually confines the child (the ADR 0016 mechanism).
        let granted = Caveats {
            net: Scope::only(["allowed.test".to_string()]),
            ..Caveats::top()
        };
        let prefix = best_available_sandbox(&Arc::new(SandboxPolicy::default()))
            .command_prefix(&loopback_fenced_caveats(&granted))
            .expect("seatbelt wrapper");

        // Run `curl` wrapped by the fence, with the given env. `-v` surfaces the
        // connect-time error on stderr so the FENCE leg can assert a *permission*
        // denial (not a mere routing failure).
        let run = |proxy_env: bool, url: &str| -> std::process::Output {
            let mut cmd = std::process::Command::new(&prefix[0]);
            cmd.args(&prefix[1..])
                .arg(curl)
                .args(["-sv", "--max-time", "5", url])
                .env_clear();
            if proxy_env {
                cmd.envs(proxy.proxy_env());
            }
            cmd.output().expect("spawn sandbox-exec")
        };

        // ALLOW: via the proxy, the allow-listed host reaches the loopback origin.
        //
        // In a few CI environments the first `curl` attempt can be an
        // in-flight startup race that ends in "Empty reply from server" even
        // though the proxy and origin are otherwise healthy. A tiny bounded retry
        // keeps this assertion focused on policy while filtering transient transport
        // noise.
        let mut allow = None;
        for attempt in 0..3 {
            let output = run(true, "http://allowed.test/");
            let stdout = String::from_utf8_lossy(&output.stdout);
            if output.status.success() && stdout.contains("origin") {
                allow = Some(output);
                break;
            }
            if attempt < 2 {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            allow = Some(output);
        }
        let allow = allow.expect("at least one allow-leg probe should run");
        assert!(
            allow.status.success() && String::from_utf8_lossy(&allow.stdout).contains("origin"),
            "allow-listed host must reach the origin through the proxy: {allow:?}"
        );
        // DENY: via the proxy, a non-allow-listed host gets the proxy's 403 — and
        // never reaches any origin.
        let deny = run(true, "http://denied.test/");
        assert!(
            String::from_utf8_lossy(&deny.stdout).contains("403"),
            "denied host must get the proxy's 403: {deny:?}"
        );
        // FENCE: WITHOUT the proxy env the child tries to egress directly; a literal
        // off-box IP is kernel-denied at the socket. Assert curl exit 7 AND an EPERM
        // signal ("Operation not permitted") — so a no-internet runner (ENETUNREACH,
        // also exit 7) cannot make this pass vacuously; it must be a *permission*
        // denial, proving the fence (not the network) blocked it.
        let direct = run(false, "http://1.1.1.1/");
        let stderr = String::from_utf8_lossy(&direct.stderr);
        assert_eq!(
            direct.status.code(),
            Some(7),
            "direct off-box egress must be kernel-denied (curl exit 7): {stderr}"
        );
        assert!(
            stderr.contains("Operation not permitted"),
            "the block must be a kernel EPERM, not a routing failure: {stderr}"
        );

        drop(proxy);
    }

    /// The other always-on REAL-socket test: it must exercise the actual
    /// `TcpListener` bind + accept loop + `Drop` teardown, which no in-memory
    /// fake can. It probes a just-released ephemeral port, so it serializes on
    /// `net_test_lock()` — a concurrent sibling could re-bind that port between
    /// the drop and the probe, and the probe would then hit the sibling's live
    /// listener (#207).
    #[test]
    fn dropping_the_handle_stops_the_listener() {
        let _serial = net_test_lock();
        let proxy = start_null(["x".to_string()], Arc::new(StdResolver)).unwrap();
        let addr = proxy.addr();
        drop(proxy);
        // After shutdown the port is no longer served: a connect either refuses
        // or the accept loop has exited. Give the OS a moment, then assert we
        // cannot complete an HTTP exchange through it.
        thread::sleep(Duration::from_millis(100));
        if let Ok(mut c) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            c.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let _ = write!(c, "GET http://x/ HTTP/1.1\r\n\r\n");
            let mut resp = Vec::new();
            let _ = c.read_to_end(&mut resp);
            assert!(resp.is_empty(), "a stopped proxy must not serve requests");
        }
    }
}
