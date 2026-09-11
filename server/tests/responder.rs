//! The responder answers exchanges a device opens.
//!
//! Everything else this server does starts with the controller talking. This
//! is the other direction, and it cannot be asserted from the WebSocket
//! contract tests: it needs a real Matter stack on each end and a real UDP
//! round-trip between them.
//!
//! The probe is a plaintext `CASESigma1`, because that is the one thing a
//! stranger may legitimately send unencrypted: rs-matter's transport creates a
//! new unsecured session only for `PBKDFParamRequest` and `CASESigma1`. It is
//! also the exchange a real device opens when it wants to reach this node, so
//! what comes back is what a device would get.
//!
//! The handshake is from a fabric this server has never heard of, which is the
//! one thing that can be arranged without commissioning: two stacks, two
//! fabrics, no shared root. The spec's answer to that is a specific status —
//! not silence, and not a generic failure — so asserting it proves the whole
//! path: the exchange was accepted, the Secure Channel handler read it, the
//! fabric table was consulted, and the answer was sent back over UDP.

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use async_io::Async;
use futures_lite::future::{block_on, or};
use rand_core::OsRng;
use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::DAC_PRIVKEY;
use rs_matter::sc::{GeneralCode, OpCode, StatusReport, PROTO_ID_SECURE_CHANNEL};
use rs_matter::tlv::{TLVTag, TLVWrite};
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::tcp::TcpNetwork;
use rs_matter::transport::network::{Address, ChainedNetwork};
use rs_matter::utils::storage::ReadBuf;
use rs_matter_server::api::{RuntimeInfo, ServerContext};
use rs_matter_server::matter::actor;
use rs_matter_server::matter::controller::{init_matter, FabricConfig};
use rs_matter_server::matter::responder;
use rs_matter_server::storage::{ConfigStore, NodeStore};

/// `SCStatusCodes::NoSharedTrustRoots`: the initiator's destination id matched
/// none of this node's fabrics. Asserted as a literal so the wire value is
/// pinned here rather than restated from the enum under test.
const SC_NO_SHARED_TRUST_ROOTS: u16 = 1;

/// Sigma1's TLV context tags, which start at 1.
const SIGMA1_INITIATOR_RANDOM: u8 = 1;
const SIGMA1_INITIATOR_SESSION_ID: u8 = 2;
const SIGMA1_DESTINATION_ID: u8 = 3;
const SIGMA1_PEER_PUBLIC_KEY: u8 = 4;

/// The shared state the responder publishes into. Nothing in this test reads
/// it — no report can arrive without a subscription — but the responder holds
/// it, so it has to be real.
fn context() -> Arc<ServerContext> {
    let (handle, requests) = actor::channel();
    // Dropping the receiving end makes any Matter op fail fast rather than
    // hang, which is what a test with no actor behind it wants.
    drop(requests);
    Arc::new(ServerContext::new(
        handle,
        Arc::new(NodeStore::new()),
        Arc::new(ConfigStore::in_memory()),
        RuntimeInfo::default(),
        "info".to_string(),
    ))
}

/// An ephemeral port for a Matter stack to answer on, over both transports.
///
/// Bound before the stack that will own it, so the port is known to the test
/// without having to ask the thread for it afterwards — and bound on UDP
/// first, since TCP can then be asked for the same number.
fn matter_sockets() -> (UdpSocket, TcpListener, u16) {
    let socket = UdpSocket::bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0))
        .expect("an ephemeral UDP port");
    let port = socket.local_addr().unwrap().port();
    let listener = TcpListener::bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0))
        .expect("the same port for TCP");
    (socket, listener, port)
}

/// A server: its transport and its responder, on their own thread, for the
/// lifetime of the test.
///
/// `Matter` is `!Send` — it holds a `dyn DeviceAttestation` — so it is built
/// on the thread that will run it, exactly as the real server does.
fn serve(storage: String, socket: UdpSocket, listener: TcpListener) {
    thread::spawn(move || {
        let port = socket.local_addr().unwrap().port();
        let matter = init_matter(
            &storage,
            &FabricConfig {
                port,
                ..FabricConfig::default()
            },
        )
        .expect("a fabric");
        let socket = Async::new(socket).expect("a non-blocking socket");
        let tcp = TcpNetwork::<2>::new(Async::new(listener).expect("a non-blocking listener"));
        let crypto = default_crypto(OsRng, DAC_PRIVKEY);
        let send = ChainedNetwork::new(Address::is_tcp, &tcp, &socket);
        let recv = ChainedNetwork::new(Address::is_tcp, &tcp, &socket);
        block_on(or(
            async {
                let _ = matter.run(&crypto, send, recv, &socket).await;
            },
            responder::run(&matter, &crypto, PathBuf::from(&storage), context()),
        ));
    });
    // Let the transport reach its first poll before anything is sent to it.
    thread::sleep(Duration::from_millis(100));
}

#[test]
fn a_handshake_from_an_unknown_fabric_is_answered_rather_than_ignored() {
    probe_over(|port| Address::Udp(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)));
}

/// The same handshake over TCP, which is the transport a payload too large for
/// MRP takes — a camera's SDP offer being the one that needs it. What is being
/// tested is that the stream is accepted, de-framed and answered at all.
#[test]
fn a_handshake_over_tcp_is_answered_too() {
    probe_over(|port| Address::Tcp(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)));
}

fn probe_over(address: impl Fn(u16) -> Address) {
    let server_dir = tempfile::tempdir().expect("tempdir");
    let client_dir = tempfile::tempdir().expect("tempdir");
    let (server_socket, server_listener, port) = matter_sockets();
    serve(
        server_dir.path().to_str().unwrap().to_string(),
        server_socket,
        server_listener,
    );

    let client_storage = client_dir.path().to_str().unwrap().to_string();
    let client_matter = init_matter(&client_storage, &FabricConfig::default()).expect("a fabric");
    let (client_socket, client_listener, _) = matter_sockets();
    let client_socket = Async::new(client_socket).expect("a non-blocking socket");
    let client_tcp =
        TcpNetwork::<2>::new(Async::new(client_listener).expect("a non-blocking listener"));

    let crypto = default_crypto(OsRng, DAC_PRIVKEY);
    let peer = address(port);

    let probe = async {
        let mut exchange = Exchange::initiate_plaintext(&client_matter, &crypto, peer)
            .await
            .expect("open an exchange to the server");

        // A well-formed Sigma1 whose destination id belongs to no fabric this
        // server holds. The randoms and the key are never examined: the
        // responder consults the fabric table first and answers before it
        // looks at them.
        exchange
            .send_with(|_, wb| {
                wb.start_struct(&TLVTag::Anonymous)?;
                wb.str(&TLVTag::Context(SIGMA1_INITIATOR_RANDOM), &[0u8; 32])?;
                wb.u16(&TLVTag::Context(SIGMA1_INITIATOR_SESSION_ID), 1)?;
                wb.str(&TLVTag::Context(SIGMA1_DESTINATION_ID), &[0u8; 32])?;
                // 0x04 marks an uncompressed EC point, which is the shape a
                // real key has.
                let mut key = [0u8; 65];
                key[0] = 0x04;
                wb.str(&TLVTag::Context(SIGMA1_PEER_PUBLIC_KEY), &key)?;
                wb.end_container()?;
                Ok(Some(OpCode::CASESigma1.meta()))
            })
            .await
            .expect("send Sigma1");

        let rx = exchange.recv().await.expect("an answer");
        let meta = rx.meta();
        assert_eq!(meta.proto_id, PROTO_ID_SECURE_CHANNEL);
        assert_eq!(
            meta.proto_opcode,
            OpCode::StatusReport as u8,
            "expected a status report, got opcode {:#04x}",
            meta.proto_opcode
        );

        let mut payload = ReadBuf::new(rx.payload());
        let report = StatusReport::read(&mut payload).expect("a readable status report");
        assert_eq!(report.proto_id, PROTO_ID_SECURE_CHANNEL as u32);
        assert_eq!(
            report.proto_code, SC_NO_SHARED_TRUST_ROOTS,
            "expected NoSharedTrustRoots, got {:?}",
            report
        );
        assert_eq!(report.general_code, GeneralCode::Failure);
    };

    // The transport has to be polled for the exchange to make progress, and a
    // failure to answer must fail the test rather than hang it.
    let timeout = async {
        async_io::Timer::after(Duration::from_secs(10)).await;
        panic!("the server never answered");
    };
    block_on(or(
        async {
            let send = ChainedNetwork::new(Address::is_tcp, &client_tcp, &client_socket);
            let recv = ChainedNetwork::new(Address::is_tcp, &client_tcp, &client_socket);
            let _ = client_matter.run(&crypto, send, recv, &client_socket).await;
        },
        or(probe, timeout),
    ));
}
