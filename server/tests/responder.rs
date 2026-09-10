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
//! also the exchange a real device opens when it wants to reach a cluster on
//! this node, so what comes back is what a device would get.
//!
//! That same rule is why the Interaction Model arms are not asserted here.
//! Reaching them needs a secured session, and this node answers `Busy` to the
//! handshake that would establish one — so until it hosts a data model there
//! is no legitimate way to put an IM message in front of it. The routing for
//! those arms is asserted directly, in `matter::responder`.

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::thread;
use std::time::Duration;

use async_io::Async;
use futures_lite::future::{block_on, or};
use rand_core::OsRng;
use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::DAC_PRIVKEY;
use rs_matter::sc::{GeneralCode, OpCode, StatusReport, PROTO_ID_SECURE_CHANNEL};
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::Address;
use rs_matter::utils::storage::ReadBuf;
use rs_matter_server::matter::controller::{init_matter, FabricConfig};
use rs_matter_server::matter::responder;

/// `SCStatusCodes::Busy`, the "try again later" a node with no cluster server
/// to reach owes an initiator. Asserted as a literal so the wire value is
/// pinned here rather than restated from the enum under test.
const SC_BUSY: u16 = 4;

/// An ephemeral UDP port for a Matter stack to answer on.
///
/// The socket is bound before the stack that will own it, so the port is known
/// to the test without having to ask the thread for it afterwards.
fn udp_socket() -> (UdpSocket, u16) {
    let socket = UdpSocket::bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0))
        .expect("an ephemeral UDP port");
    let port = socket.local_addr().unwrap().port();
    (socket, port)
}

/// A server: its transport and its responder, on their own thread, for the
/// lifetime of the test.
///
/// `Matter` is `!Send` — it holds a `dyn DeviceAttestation` — so it is built
/// on the thread that will run it, exactly as the real server does.
fn serve(storage: String, socket: UdpSocket) {
    thread::spawn(move || {
        let matter = init_matter(&storage, &FabricConfig::default()).expect("a fabric");
        let socket = Async::new(socket).expect("a non-blocking socket");
        let crypto = default_crypto(OsRng, DAC_PRIVKEY);
        block_on(or(
            async {
                let _ = matter.run(&crypto, &socket, &socket, &socket).await;
            },
            responder::run(&matter),
        ));
    });
    // Let the transport reach its first poll before anything is sent to it.
    thread::sleep(Duration::from_millis(100));
}

#[test]
fn a_handshake_this_node_cannot_host_is_answered_rather_than_ignored() {
    let server_dir = tempfile::tempdir().expect("tempdir");
    let client_dir = tempfile::tempdir().expect("tempdir");
    let (server_socket, port) = udp_socket();
    serve(
        server_dir.path().to_str().unwrap().to_string(),
        server_socket,
    );

    let client_storage = client_dir.path().to_str().unwrap().to_string();
    let client_matter = init_matter(&client_storage, &FabricConfig::default()).expect("a fabric");
    let (client_socket, _) = udp_socket();
    let client_socket = Async::new(client_socket).expect("a non-blocking socket");

    let crypto = default_crypto(OsRng, DAC_PRIVKEY);
    let peer = Address::Udp(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port));

    let probe = async {
        let mut exchange = Exchange::initiate_plaintext(&client_matter, &crypto, peer)
            .await
            .expect("open an exchange to the server");

        // The payload is never read: what is being tested is that something
        // accepts the exchange and answers the opcode.
        exchange
            .send_with(|_, _wb| Ok(Some(OpCode::CASESigma1.meta())))
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
        assert_eq!(report.general_code, GeneralCode::Busy);
        assert_eq!(report.proto_id, PROTO_ID_SECURE_CHANNEL as u32);
        assert_eq!(report.proto_code, SC_BUSY);
    };

    // The transport has to be polled for the exchange to make progress, and a
    // failure to answer must fail the test rather than hang it.
    let timeout = async {
        async_io::Timer::after(Duration::from_secs(10)).await;
        panic!("the server never answered");
    };
    block_on(or(
        async {
            let _ = client_matter
                .run(&crypto, &client_socket, &client_socket, &client_socket)
                .await;
        },
        or(probe, timeout),
    ));
}
