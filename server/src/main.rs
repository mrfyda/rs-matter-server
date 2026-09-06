//! Binary entry point.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;

use rs_matter_server::api::RuntimeInfo;
use rs_matter_server::matter::controller::{init_controller, FabricConfig};
use rs_matter_server::monitor::MonitorConfig;
use rs_matter_server::storage::{ConfigStore, NodeStore};
use rs_matter_server::ws::{self, ServerConfig};

#[derive(Parser, Debug)]
#[command(name = "rs-matter-server", about = "Rust Matter controller server")]
struct Args {
    /// Address the WebSocket and HTTP endpoints listen on.
    #[arg(long, env = "LISTEN_ADDRESS", default_value = "0.0.0.0:5580")]
    listen: SocketAddr,

    /// Directory holding the fabric, node, and configuration state.
    #[arg(long, env = "STORAGE_PATH", default_value = "/data")]
    storage_path: String,

    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Pin the fabric label, ignoring `set_default_fabric_label`.
    #[arg(long, env = "DEFAULT_FABRIC_LABEL")]
    default_fabric_label: Option<String>,

    /// Turn off firmware update support and the upload endpoint.
    #[arg(long, env = "DISABLE_OTA", default_value_t = false)]
    disable_ota: bool,

    /// Turn off Thread diagnostics collection.
    #[arg(long, env = "DISABLE_THREAD_DIAGNOSTICS", default_value_t = false)]
    disable_thread_diagnostics: bool,

    /// How often, in seconds, a reachable node is re-read for attribute
    /// changes.
    #[arg(long, env = "POLL_INTERVAL_SECS", default_value_t = 30)]
    poll_interval_secs: u64,

    /// Also consult the CSA test ledger for devices with test vendor ids.
    #[arg(long, env = "ENABLE_TEST_NET_DCL", default_value_t = false)]
    enable_test_net_dcl: bool,

    /// Probe a running server's `/health` endpoint and exit. Used as the
    /// container health check, so it deliberately starts nothing.
    #[arg(long, default_value_t = false)]
    health_check: bool,
}

/// Ask a running server whether it is serving.
///
/// A plain TCP connect would only prove something is bound to the port; this
/// requires a well-formed response from the connection handler.
fn health_check(listen: SocketAddr) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    // The listen address may be a wildcard, which cannot be connected to.
    let target = if listen.ip().is_unspecified() {
        SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            listen.port(),
        )
    } else {
        listen
    };

    let mut stream =
        std::net::TcpStream::connect_timeout(&target, std::time::Duration::from_secs(5))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target
    )?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    if !response.starts_with("HTTP/1.1 200") {
        anyhow::bail!(
            "health check failed: {}",
            response.lines().next().unwrap_or("")
        );
    }
    println!(
        "{}",
        response.rsplit("\r\n\r\n").next().unwrap_or("").trim()
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.health_check {
        return health_check(args.listen);
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&args.log_level))
        .init();

    log::info!("rs-matter-server starting on {}", args.listen);
    log::info!("Storage path: {}", args.storage_path);

    let controller =
        init_controller(&args.storage_path, &FabricConfig::default()).map_err(|e| {
            anyhow::anyhow!(
                "Could not initialize the Matter controller with storage at '{}': {:?}. \
             That directory holds the fabric and must be writable by the user the \
             server runs as (uid 65532 in the container image).",
                args.storage_path,
                e
            )
        })?;

    let storage = std::path::Path::new(&args.storage_path);
    let nodes = Arc::new(NodeStore::load(storage.join("nodes.json"))?);
    let config = Arc::new(ConfigStore::load(storage.join("config.json"))?);

    // A snapshot written by an older build may hold ids above the counter;
    // reserving past them keeps allocation monotonic across upgrades.
    if let Some(highest) = nodes.highest_node_id() {
        config.reserve_node_ids_above(highest)?;
    }

    if let Some(label) = &args.default_fabric_label {
        log::info!("Fabric label is pinned to '{}'", label);
    }
    log::info!("Restored {} commissioned node(s)", nodes.len());

    let server = ServerConfig {
        listen: args.listen,
        runtime: RuntimeInfo {
            // rs-matter has no BLE transport, so commissioning finds devices
            // over the IP network only.
            bluetooth_enabled: false,
            ble_proxy_enabled: false,
            ota_enabled: !args.disable_ota,
            thread_diagnostics_enabled: !args.disable_thread_diagnostics,
            pinned_fabric_label: args.default_fabric_label.clone(),
            test_net_dcl: args.enable_test_net_dcl,
            ..RuntimeInfo::default()
        },
        console_loglevel: args.log_level.clone(),
        monitor: MonitorConfig {
            interval: std::time::Duration::from_secs(args.poll_interval_secs.max(5)),
            ..MonitorConfig::default()
        },
    };

    async_std::task::block_on(ws::run(server, controller, nodes, config))
}
