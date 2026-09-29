//! #372 downstream proof (the #370 prerequisite): an async caller must
//! finalize the blocking `net_proxy` off the Tokio reactor, and no execution
//! terminal may be emitted before the proxy reaches Quiescent.
//!
//! This does NOT implement #370's execution-lifecycle manager — #370 is a
//! separate, still-open issue. It proves the exact pattern that manager will
//! have to use, against the REAL `agent_bridle_core::net_proxy` machinery, on
//! a current-thread Tokio runtime — the tightest case: if
//! `ProxyHandle::shutdown_and_join` ran directly on the reactor thread instead
//! of via `spawn_blocking`, the independent heartbeat task below could never
//! advance for the whole (multi-hundred-millisecond) duration of finalize.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_bridle_core::net_proxy::{self, NullSink, Resolver};

/// Maps every proxied host to one fixed loopback address — hermetic, no real
/// DNS or network.
struct FixedResolver(SocketAddr);
impl Resolver for FixedResolver {
    fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<SocketAddr> {
        Ok(self.0)
    }
}

#[test]
fn async_caller_finalizes_off_the_reactor_and_orders_terminal_after_quiescence() {
    // A real loopback origin that accepts and then holds the connection open —
    // the "occupied worker" the proxy must still be finalizing while the
    // reactor keeps advancing.
    let origin_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let origin_addr = origin_listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in origin_listener.incoming().flatten() {
            std::mem::forget(s); // hold the socket open; never read or write
        }
    });

    let proxy = net_proxy::start(
        ["allowed.test".to_string()],
        Arc::new(FixedResolver(origin_addr)),
        Arc::new(NullSink),
    )
    .expect("bind loopback");

    // Occupy one worker with an idle tunnel — nothing flows either way.
    let mut tunnel = TcpStream::connect(proxy.addr()).unwrap();
    tunnel
        .write_all(b"CONNECT allowed.test:443 HTTP/1.1\r\nHost: allowed.test:443\r\n\r\n")
        .unwrap();
    tunnel
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reply = [0u8; 64];
    let n = tunnel.read(&mut reply).unwrap();
    assert!(
        String::from_utf8_lossy(&reply[..n]).starts_with("HTTP/1.1 200"),
        "the tunnel must open before it goes idle"
    );

    // Record a denial too, so the terminal evidence below is not trivially
    // empty — proof point 3 needs something to carry.
    let mut denied = TcpStream::connect(proxy.addr()).unwrap();
    denied
        .write_all(b"CONNECT off-list.test:443 HTTP/1.1\r\n\r\n")
        .unwrap();
    denied
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut resp = String::new();
    let _ = denied.read_to_string(&mut resp);
    assert!(
        resp.contains("403"),
        "the denial must land before finalize starts: {resp:?}"
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build a current-thread runtime");

    let heartbeat = Arc::new(AtomicU32::new(0));
    let terminal_emitted = Arc::new(AtomicBool::new(false));

    let (before, after, evidence) = rt.block_on(async {
        let hb = Arc::clone(&heartbeat);
        let hb_task = tokio::spawn(async move {
            loop {
                hb.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let before = heartbeat.load(Ordering::SeqCst);
        let terminal_check = Arc::clone(&terminal_emitted);
        // The pattern #370's execution manager must use: the blocking
        // finalizer runs off the reactor via `spawn_blocking`, and nothing
        // marks the execution terminal until it returns.
        let evidence = tokio::task::spawn_blocking(move || {
            assert!(
                !terminal_check.load(Ordering::SeqCst),
                "no execution terminal may precede proxy quiescence"
            );
            let result = proxy.shutdown_and_join();
            assert!(
                !terminal_check.load(Ordering::SeqCst),
                "the terminal must still not exist while finalize is in flight"
            );
            result
        })
        .await
        .expect("the blocking finalize task must not panic")
        .expect("finalize must succeed (one idle tunnel + one denial, no faults)");

        // ONLY NOW — after proxy quiescence — may the terminal be emitted.
        terminal_emitted.store(true, Ordering::SeqCst);
        let after = heartbeat.load(Ordering::SeqCst);
        hb_task.abort();
        (before, after, evidence)
    });

    assert!(
        after > before,
        "an independent Tokio heartbeat must keep advancing while the blocking \
         finalizer runs off-reactor (before={before}, after={after})"
    );
    assert!(
        terminal_emitted.load(Ordering::SeqCst),
        "the terminal must be emitted after quiescence"
    );
    assert_eq!(
        evidence.shutdown_aborts, 1,
        "the terminal's evidence must reflect the occupied worker's forced abort: {evidence:?}"
    );
    assert!(
        evidence
            .refused_hosts
            .contains(&"off-list.test".to_string()),
        "the terminal's evidence must carry the denial recorded before finalize: {evidence:?}"
    );
    // `evidence` is an owned value moved out of a consumed `ProxyHandle` — there
    // is no live handle left anywhere that could mutate it further; nothing
    // beyond this point changes it. That is the "no evidence changes after the
    // terminal" guarantee, structurally rather than by a redundant re-check.
}
