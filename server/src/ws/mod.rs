//! The server: listener, Matter runtime, and connection dispatch.
//!
//! Everything that touches `Matter` — the transport, mDNS, and the controller
//! actor — is polled on one executor thread, because `Matter` is `!Send`.
//! Connections run on their own threads and reach it only through the actor's
//! channel.

pub mod connection;
pub mod http;

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::Result;
use async_io::Async;
use futures_lite::future::block_on;

use rand_core::OsRng;
use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::devices::test::DAC_PRIVKEY;
#[cfg(target_os = "macos")]
use rs_matter::transport::network::mdns::astro::AstroMdns;
#[cfg(not(target_os = "macos"))]
use rs_matter::transport::network::mdns::builtin::{BuiltinMdns, Host};

use crate::api::{RuntimeInfo, ServerContext};
use crate::matter::actor::{self, ActorContext};
use crate::matter::controller::MatterController;
use crate::monitor::{self, MonitorConfig};
use crate::protocol::events::Event;
use crate::storage::{ConfigStore, NodeStore};

/// Everything the server needs to start.
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub runtime: RuntimeInfo,
    pub console_loglevel: String,
    pub monitor: MonitorConfig,
}

/// Start the server and run until the Matter transport stops.
pub async fn run(
    config: ServerConfig,
    controller: MatterController,
    nodes: Arc<NodeStore>,
    store: Arc<ConfigStore>,
) -> Result<()> {
    let listener = TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let listener = Async::new(listener)?;

    let MatterController {
        matter,
        icac_private_key,
        storage_path,
    } = controller;

    let matter_socket =
        Async::<UdpSocket>::bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0))?;
    let crypto = default_crypto(OsRng, DAC_PRIVKEY);

    let (handle, requests) = actor::channel();
    let monitor_config = config.monitor;
    let context = Arc::new(ServerContext::new(
        handle,
        nodes,
        store,
        config.runtime,
        config.console_loglevel,
    ));

    // Attribute monitoring runs off the Matter thread: it holds only the actor
    // handle, so it queues work exactly as a connection does.
    {
        let context = context.clone();
        thread::spawn(move || block_on(monitor::run(context, monitor_config)));
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    // rs-matter's crypto backend is not `Clone`, but `&C` implements `Crypto`
    // and is `Copy`, so the actor holds a reference and the transport keeps
    // the backend itself.
    let actor_context = ActorContext {
        matter: &matter,
        crypto: &crypto,
        icac_private_key: &icac_private_key,
        storage_path,
    };

    // mDNS is set up before the run loop rather than inside it. Matter cannot
    // work without it — devices are found and reached by mDNS — so failing to
    // start it is a startup error, and the operator needs to be told why
    // instead of watching the process exit after a lone log line.
    let mdns = prepare_mdns()?;

    // `or` polls both branches and returns when either finishes. The transport,
    // mDNS, actor, and accept loops are all long-lived, so any of them exiting
    // ends the server — which is what should happen if the radio stops.
    let network = futures_lite::future::or(
        run_transport(&matter, &crypto, &matter_socket),
        run_mdns(&matter, &crypto, &mdns),
    );
    let work = futures_lite::future::or(
        actor::run(actor_context, requests),
        accept_loop(listener, shutdown.clone(), context.clone()),
    );
    futures_lite::future::or(network, work).await;

    // Tell every listening client before the socket goes away; a client that
    // sees this reconnects instead of waiting for a timeout.
    context.events.publish(Event::server_shutdown());
    shutdown.store(true, Ordering::SeqCst);
    Ok(())
}

async fn run_transport<C: Crypto>(
    matter: &rs_matter::Matter<'_>,
    crypto: &C,
    socket: &Async<UdpSocket>,
) {
    if let Err(error) = matter.run(crypto, socket, socket, socket).await {
        log::error!("Matter transport stopped: {:?}", error);
    }
}

/// What mDNS needs before the run loop starts. macOS delegates to the system
/// responder and so has nothing to prepare.
#[cfg(target_os = "macos")]
struct MdnsSetup;

#[cfg(target_os = "macos")]
fn prepare_mdns() -> Result<MdnsSetup> {
    Ok(MdnsSetup)
}

#[cfg(target_os = "macos")]
async fn run_mdns<C: Crypto>(matter: &rs_matter::Matter<'_>, _crypto: &C, _setup: &MdnsSetup) {
    // macOS routes mDNS through the system responder; a second responder on
    // port 5353 would conflict with it.
    let mut mdns = AstroMdns::new();
    if let Err(error) = mdns.run(matter).await {
        log::error!("mDNS stopped: {:?}", error);
    }
}

/// The interface and socket the responder runs on.
#[cfg(not(target_os = "macos"))]
struct MdnsSetup {
    interface: MdnsInterface,
    socket: Async<UdpSocket>,
    hostname: String,
}

/// Choose an interface and bind the responder's socket.
///
/// Matter is IPv6-first, so this is one dual-stack socket joined to both the
/// IPv4 and IPv6 mDNS groups. `SO_REUSEPORT` matters in deployment rather than
/// in tests: hosts that matter here — Raspberry Pi OS, Home Assistant OS — run
/// `avahi-daemon`, which already holds port 5353.
///
/// Both failures are diagnosed rather than logged and shrugged off. Matter
/// finds and reaches devices over mDNS, so a server that cannot start it is
/// not going to work, and the operator needs to know why.
#[cfg(not(target_os = "macos"))]
fn prepare_mdns() -> Result<MdnsSetup> {
    let interface = select_interface().map_err(|error| {
        anyhow::anyhow!(
            "Cannot start mDNS: {error}. Matter finds and reaches devices over mDNS \
             on an IPv6-capable interface, so the server cannot run without one. In \
             a container this usually means bridge networking — run it with host \
             networking instead."
        )
    })?;
    log::info!(
        "Using interface {} ({} / {}) for mDNS",
        interface.name,
        interface.ipv4,
        interface.ipv6
    );

    let socket = bind_mdns_socket(&interface).map_err(|error| {
        anyhow::anyhow!(
            "Cannot bind the mDNS socket on port 5353: {error}. Another mDNS responder \
             may be holding it without SO_REUSEPORT."
        )
    })?;

    Ok(MdnsSetup {
        interface,
        socket,
        hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "rs-matter-server".to_string()),
    })
}

#[cfg(not(target_os = "macos"))]
async fn run_mdns<C: Crypto>(matter: &rs_matter::Matter<'_>, crypto: &C, setup: &MdnsSetup) {
    let host = Host {
        hostname: &setup.hostname,
        ip: setup.interface.ipv4,
        ipv6: setup.interface.ipv6,
    };
    if let Err(error) = BuiltinMdns::new()
        .run(
            &setup.socket,
            &setup.socket,
            &host,
            Some(setup.interface.ipv4),
            Some(setup.interface.index),
            matter,
            crypto,
        )
        .await
    {
        log::error!("mDNS stopped: {:?}", error);
    }
}

/// The interface mDNS advertises and listens on.
#[cfg(not(target_os = "macos"))]
struct MdnsInterface {
    name: String,
    index: u32,
    ipv4: std::net::Ipv4Addr,
    ipv6: Ipv6Addr,
}

/// Pick the first interface that is up, not loopback, and has both an IPv4 and
/// an IPv6 address — Matter needs IPv6 to reach devices, and mDNS needs IPv4
/// for the v4 group.
///
/// `getifaddrs` reports one entry per address, so an interface with both
/// families appears more than once. This walks the list once and groups the
/// entries by interface, keeping the first address of each family — the order
/// the OS gives them in. Interfaces are then considered in the order they were
/// first seen.
#[cfg(not(target_os = "macos"))]
fn select_interface() -> anyhow::Result<MdnsInterface> {
    use nix::net::if_::InterfaceFlags;

    /// An interface as it is being assembled from its addresses. Either family
    /// may still be missing, in which case the interface is unusable.
    struct Candidate {
        name: String,
        ipv4: Option<std::net::Ipv4Addr>,
        ipv6: Option<Ipv6Addr>,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for entry in nix::ifaddrs::getifaddrs()? {
        if !entry.flags.contains(InterfaceFlags::IFF_UP)
            || entry
                .flags
                .intersects(InterfaceFlags::IFF_LOOPBACK | InterfaceFlags::IFF_POINTOPOINT)
        {
            continue;
        }

        let Some(address) = entry.address.as_ref() else {
            continue;
        };

        // Found by index rather than `iter_mut().find()`, which would hold the
        // borrow across the push in the `None` arm.
        let candidate = match candidates
            .iter()
            .position(|candidate| candidate.name == entry.interface_name)
        {
            Some(index) => &mut candidates[index],
            None => {
                candidates.push(Candidate {
                    name: entry.interface_name.clone(),
                    ipv4: None,
                    ipv6: None,
                });
                candidates.last_mut().expect("just pushed")
            }
        };

        if let Some(ipv4) = address.as_sockaddr_in() {
            candidate.ipv4.get_or_insert(ipv4.ip());
        } else if let Some(ipv6) = address.as_sockaddr_in6() {
            candidate.ipv6.get_or_insert(ipv6.ip());
        }
    }

    candidates
        .into_iter()
        .find_map(|candidate| {
            let ipv4 = candidate.ipv4?;
            let ipv6 = candidate.ipv6?;
            Some(MdnsInterface {
                index: nix::net::if_::if_nametoindex(candidate.name.as_str()).unwrap_or(0),
                name: candidate.name,
                ipv4,
                ipv6,
            })
        })
        .ok_or_else(|| anyhow::anyhow!("no interface is up with both an IPv4 and an IPv6 address"))
}

#[cfg(not(target_os = "macos"))]
fn bind_mdns_socket(interface: &MdnsInterface) -> anyhow::Result<Async<UdpSocket>> {
    use rs_matter::transport::network::mdns::{
        MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
    };
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // Lets the responder share port 5353 with the host's own mDNS daemon.
    socket.set_reuse_port(true)?;
    // Dual stack, so one socket carries both mDNS groups.
    socket.set_only_v6(false)?;
    socket.bind(&MDNS_SOCKET_DEFAULT_BIND_ADDR.into())?;

    let socket = Async::<UdpSocket>::new_nonblocking(socket.into())?;
    socket
        .get_ref()
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, interface.index)?;
    socket
        .get_ref()
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &interface.ipv4)?;
    Ok(socket)
}

/// Accept connections and hand each to its own thread.
///
/// The loop wakes periodically even when idle so a shutdown request is noticed
/// without needing a connection to arrive first.
async fn accept_loop(
    listener: Async<TcpListener>,
    shutdown: Arc<AtomicBool>,
    context: Arc<ServerContext>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        let accept = async { listener.accept().await.map(|(stream, _)| stream) };
        let tick = async {
            async_io::Timer::after(std::time::Duration::from_millis(200)).await;
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "tick"))
        };

        let stream = match futures_lite::future::or(accept, tick).await {
            Ok(stream) => stream,
            Err(_) => continue,
        };

        let peer = stream
            .get_ref()
            .peer_addr()
            .map(|address| address.to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let stream: TcpStream = match stream.into_inner() {
            Ok(stream) => stream,
            Err(error) => {
                log::debug!("{}: could not take the accepted socket: {}", peer, error);
                continue;
            }
        };

        let context = context.clone();
        thread::spawn(move || {
            let Ok(stream) = Async::new(stream) else {
                return;
            };
            block_on(connection::serve(stream, peer, context));
        });
    }
}

/// A server bound to an ephemeral port with no Matter behind it, for protocol
/// tests.
pub struct TestServer {
    pub port: u16,
    pub context: Arc<ServerContext>,
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start a test server. The Matter actor is absent, so commands that need a
/// radio fail fast with the SDK error while the protocol surface stays live.
pub async fn test_start() -> TestServer {
    test_start_with(|context| context).await
}

/// Start a test server, adjusting the context first (seeding nodes, say).
pub async fn test_start_with(prepare: impl FnOnce(ServerContext) -> ServerContext) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener.set_nonblocking(true).expect("set non-blocking");
    let port = listener.local_addr().unwrap().port();

    let (handle, requests) = actor::channel();
    drop(requests);

    let context = ServerContext::new(
        handle,
        Arc::new(NodeStore::new()),
        Arc::new(ConfigStore::in_memory()),
        RuntimeInfo::default(),
        "info".to_string(),
    );
    context.set_fabric_info(actor::FabricInfo {
        fabric_id: 1,
        compressed_fabric_id: 0x1234_5678_9ABC_DEF0,
        fabric_index: 1,
        node_id: 112233,
        vendor_id: 0xFFF1,
        label: "HomeAssistant".to_string(),
    });
    let context = Arc::new(prepare(context));

    let shutdown = Arc::new(AtomicBool::new(false));
    let join = {
        let context = context.clone();
        let shutdown = shutdown.clone();
        thread::spawn(move || {
            let listener = Async::new(listener).expect("wrap the listener");
            block_on(accept_loop(listener, shutdown, context));
        })
    };

    // Give the accept loop a moment to reach its first poll.
    async_io::Timer::after(std::time::Duration::from_millis(50)).await;
    TestServer {
        port,
        context,
        shutdown,
        join: Some(join),
    }
}
