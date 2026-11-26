//! Real sockets ground the injected endpoint-lifetime tests: a missing hello
//! reaches the read timeout, and transferred sockets retain kernel identity.
//! Run explicitly with `--broker-transport-real`; cases execute sequentially.

// Compile the production modules unchanged so this harness can exercise the
// private transport without exporting test-only library APIs or a second wire
// implementation. The unrelated worker-control APIs are unused here.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(
    dead_code,
    reason = "same-image harness reuses private production modules"
)]
#[path = "../src/broker_transport.rs"]
mod broker_transport;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(
    dead_code,
    reason = "same-image harness reuses private production modules"
)]
#[path = "../src/command_broker.rs"]
mod command_broker;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(
    dead_code,
    reason = "same-image harness reuses private production modules"
)]
#[path = "../src/private_control.rs"]
#[allow(
    unused_imports,
    reason = "harnessless build omits imported unit test functions"
)]
mod private_control;

fn main() {
    if std::env::args().any(|arg| arg == "--list") {
        if std::env::args().any(|arg| arg == "--ignored") {
            println!("broker_transport_real: test");
        }
        return;
    }
    if !std::env::args().any(|arg| arg == "--broker-transport-real" || arg == "--ignored") {
        eprintln!("broker_transport_real requires --broker-transport-real (real sockets)");
        return;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    run();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    panic!("broker transport requires Linux or macOS");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use agent_bridle_core::receive_control_endpoint;
    use broker_transport::{BrokerClient, RpcReply};
    use command_broker::BrokerControl;

    // Grounds transferred_endpoint_closes_on_every_hello_failure: retaining
    // the sent end masks EOF when the receiver dies before writing its hello.
    let (sender, receiver) = UnixStream::pair().unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            drop(receive_control_endpoint(&receiver).unwrap());
        });
        let error = BrokerClient::new(sender)
            .request_inner(b"abandoned", Duration::from_millis(50))
            .err()
            .expect("missing hello must time out");
        server.join().unwrap();
        let message = error.to_string();
        assert!(message.contains("read peer handshake"), "{message}");
        assert!(
            message.contains("kind=WouldBlock") || message.contains("kind=TimedOut"),
            "{message}"
        );
        println!("server_died_before_hello: finite read timeout, no EOF ... ok");
    });

    // Grounds transferred_endpoint_outlives_hello_and_both_identity_lookups
    // with the real production RPC and reply-endpoint acknowledgement. Darwin
    // executes LOCAL_PEERPID + LOCAL_PEERTOKEN; Linux is a plumbing control.
    let iterations: usize = std::env::args()
        .find_map(|arg| {
            arg.strip_prefix("--iterations=")
                .map(|n| n.parse().unwrap())
        })
        .unwrap_or(300_000);
    assert!(iterations > 0);
    let (sender, receiver) = UnixStream::pair().unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            for _ in 0..iterations {
                std::thread::yield_now();
                let stream = receive_control_endpoint(&receiver).unwrap();
                let control = BrokerControl {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    deadline: Instant::now() + Duration::from_secs(30),
                };
                let (mut peer, endpoint) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                broker_transport::serve_request(
                    stream,
                    &control,
                    Some(std::process::id()),
                    |_, payload| {
                        assert_eq!(payload, b"request");
                        Ok(RpcReply {
                            payload: b"reply".to_vec(),
                            endpoint: Some(endpoint),
                        })
                    },
                )
                .unwrap();
                let mut receipt = [0];
                peer.read_exact(&mut receipt).unwrap();
                assert_eq!(receipt, [7]);
            }
        });
        let client = BrokerClient::new(sender);
        for iteration in 0..iterations {
            let reply = client
                .request_inner(b"request", Duration::from_secs(30))
                .unwrap_or_else(|error| panic!("handshake {iteration}: {error}"));
            assert_eq!(reply.payload, b"reply");
            reply.endpoint.unwrap().write_all(&[7]).unwrap();
        }
        server.join().unwrap();
    });
    println!(
        "broker_handshakes os={} iterations={iterations} ENOTCONN=0 other_errors=0",
        std::env::consts::OS
    );
}
