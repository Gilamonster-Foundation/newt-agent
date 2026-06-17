//! Health probing — the I/O layer that drives a [`BackendPool`]'s liveness.
//!
//! The probe is behind a [`Prober`] trait so the pool's refresh logic stays pure
//! and unit-testable (a mock prober), while the real impl does network I/O. The
//! first concrete prober is a dependency-free TCP reachability check; an HTTP
//! `/api/tags` prober (which would also refresh the model inventory — the
//! "breathing pool" data) is a later refinement.

use crate::{BackendPool, Health, PoolBackend};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Determines a backend's current [`Health`]. Injected into
/// [`BackendPool::refresh_health`] so the refresh logic is testable without I/O.
pub trait Prober {
    /// Probe one backend and return its current health.
    fn probe(&self, backend: &PoolBackend) -> Health;
}

/// A reachability prober: opens a TCP connection to the backend's `host:port`
/// within a timeout. `Up` on connect, `Down` on failure/timeout/unresolvable.
///
/// This checks *reachability*, not that the inference server is actually serving
/// (a TCP connect to a Traefik front proves the front is up, not Ollama behind
/// it). It never reports `Busy` — that is driven by dispatch feedback (a request
/// timeout marks a backend `Busy`), not by reachability.
#[derive(Debug, Clone, Copy)]
pub struct TcpProber {
    /// Per-connect timeout.
    pub timeout: Duration,
}

impl TcpProber {
    /// A prober with the given per-connect timeout.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for TcpProber {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2),
        }
    }
}

impl Prober for TcpProber {
    fn probe(&self, backend: &PoolBackend) -> Health {
        match host_port(&backend.endpoint) {
            Some((host, port)) if tcp_reachable(&host, port, self.timeout) => Health::Up,
            _ => Health::Down,
        }
    }
}

/// Parse `scheme://host[:port][/path]` into `(host, port)`, defaulting the port by
/// scheme (https→443, else→80). Returns `None` for an unparseable endpoint.
fn host_port(endpoint: &str) -> Option<(String, u16)> {
    let (scheme, rest) = match endpoint.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("http", endpoint),
    };
    // Drop any path/query: the authority ends at the first '/'.
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let default_port = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port.parse().ok()?;
            Some((host.to_string(), port))
        }
        Some(_) => None, // ":1234" with no host
        None => Some((authority.to_string(), default_port)),
    }
}

/// Whether `host:port` accepts a TCP connection within `timeout`.
fn tcp_reachable(host: &str, port: u16, timeout: Duration) -> bool {
    match (host, port).to_socket_addrs() {
        Ok(mut addrs) => addrs
            .next()
            .is_some_and(|addr| TcpStream::connect_timeout(&addr, timeout).is_ok()),
        Err(_) => false,
    }
}

impl BackendPool {
    /// Re-probe every backend and update its [`Health`]. Returns how many changed —
    /// the "breathing pool" tick: backends that went down drop out of candidate
    /// selection, ones that came back rejoin.
    pub fn refresh_health(&mut self, prober: &dyn Prober) -> usize {
        let mut changed = 0;
        for b in &mut self.backends {
            let h = prober.probe(b);
            if h != b.health {
                b.health = h;
                changed += 1;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendKind, StaticSource};
    use std::collections::HashMap;
    use std::net::TcpListener;

    fn backend(name: &str, endpoint: &str) -> PoolBackend {
        PoolBackend::new(name, endpoint, BackendKind::Ollama)
    }

    #[test]
    fn host_port_parses_schemes_ports_and_paths() {
        assert_eq!(
            host_port("http://localhost:11434"),
            Some(("localhost".into(), 11434))
        );
        assert_eq!(
            host_port("https://dgx-ollama.home.lab"),
            Some(("dgx-ollama.home.lab".into(), 443))
        );
        assert_eq!(host_port("http://host"), Some(("host".into(), 80)));
        assert_eq!(
            host_port("https://h:8443/v1/chat"),
            Some(("h".into(), 8443))
        );
        assert_eq!(
            host_port("bare-host:1234"),
            Some(("bare-host".into(), 1234))
        );
        assert_eq!(host_port(""), None);
        assert_eq!(host_port("http://"), None);
        assert_eq!(host_port("http://h:notaport"), None);
    }

    #[test]
    fn tcp_prober_up_on_a_live_listener_down_on_a_closed_port() {
        // Bind a real listener → reachable → Up.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let up = backend("live", &format!("http://127.0.0.1:{}", addr.port()));
        let prober = TcpProber::new(Duration::from_millis(500));
        assert_eq!(prober.probe(&up), Health::Up);

        // Port 1 on loopback is refused fast → Down.
        let down = backend("dead", "http://127.0.0.1:1");
        assert_eq!(prober.probe(&down), Health::Down);

        // Unresolvable host (.invalid is reserved as non-resolvable) → Down.
        let bogus = backend("bogus", "http://nope.invalid:80");
        assert_eq!(prober.probe(&bogus), Health::Down);

        // Unparseable endpoint → Down.
        let bad = backend("bad", "");
        assert_eq!(prober.probe(&bad), Health::Down);
    }

    /// A deterministic prober for testing the pool refresh logic without I/O.
    struct MockProber(HashMap<String, Health>);
    impl Prober for MockProber {
        fn probe(&self, b: &PoolBackend) -> Health {
            self.0.get(&b.name).copied().unwrap_or(Health::Down)
        }
    }

    #[test]
    fn refresh_health_updates_pool_and_counts_changes() {
        let mut pool = BackendPool::from_source(&StaticSource {
            backends: vec![
                backend("dgx", "http://dgx").with_health(Health::Up),
                backend("gnuc", "http://gnuc").with_health(Health::Up),
            ],
        });
        let mock = MockProber(HashMap::from([
            ("dgx".to_string(), Health::Up),    // unchanged
            ("gnuc".to_string(), Health::Down), // changed Up→Down
        ]));
        assert_eq!(pool.refresh_health(&mock), 1, "only gnuc changed");
        // gnuc dropped out of candidate selection; dgx still live.
        let names: Vec<_> = pool
            .backends()
            .iter()
            .map(|b| (b.name.as_str(), b.health))
            .collect();
        assert_eq!(names, vec![("dgx", Health::Up), ("gnuc", Health::Down)]);
        // Idempotent: a second identical probe changes nothing.
        assert_eq!(pool.refresh_health(&mock), 0);
    }

    #[test]
    fn default_prober_has_a_timeout() {
        assert_eq!(TcpProber::default().timeout, Duration::from_secs(2));
    }
}
