//! Binary entry point.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use rs_matter_server::api::RuntimeInfo;
use rs_matter_server::matter::controller::{
    init_controller_with_import, FabricConfig, FabricOrigin,
};
use rs_matter_server::monitor::MonitorConfig;
use rs_matter_server::storage::{ConfigStore, NodeStore};
use rs_matter_server::ws::{self, ServerConfig};

#[derive(Parser, Debug)]
#[command(
    name = "rs-matter-server",
    version = rs_matter_server::api::VERSION,
    about = "Rust Matter controller server"
)]
struct Args {
    /// Address the WebSocket and HTTP endpoints listen on.
    #[arg(long, env = "LISTEN_ADDRESS", default_value = "0.0.0.0:5580")]
    listen: SocketAddr,

    /// Directory holding the fabric, node, and configuration state.
    #[arg(long, env = "STORAGE_PATH", default_value = "/data")]
    storage_path: String,

    /// The port this node answers Matter traffic on, and advertises to
    /// devices.
    ///
    /// The default is Matter's own port. Move it only if something else on
    /// this host has it — another Matter server, most likely — remembering
    /// that a device caches what it resolved, so changing it after
    /// commissioning makes this node briefly unreachable.
    #[arg(long, env = "MATTER_PORT", default_value_t = rs_matter_server::matter::controller::MATTER_PORT)]
    matter_port: u16,

    /// Adopt the fabric, nodes and settings of a matterjs-server installation
    /// on first start, so its devices do not have to be re-commissioned.
    ///
    /// Point this at that server's `--storage-path`. The directory is only
    /// read; the import is skipped once this server has a fabric of its own,
    /// so the flag is safe to leave in place.
    #[arg(long, env = "IMPORT_MATTERJS")]
    import_matterjs: Option<PathBuf>,

    /// Which matter.js storage namespace holds the fabric to import. Only
    /// needed for a multi-fabric source, where `server` is not the name.
    #[arg(long, env = "IMPORT_MATTERJS_NAMESPACE")]
    import_matterjs_namespace: Option<String>,

    /// Report what `--import-matterjs` would adopt, then exit without
    /// starting the server or writing anything.
    #[arg(long, default_value_t = false)]
    import_matterjs_dry_run: bool,

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

    /// Turn off Bluetooth commissioning, even where an adapter is available.
    ///
    /// Bluetooth is used on its own when the build has the `bluetooth`
    /// feature and BlueZ offers an adapter, so this only exists to say no.
    #[arg(long, env = "DISABLE_BLUETOOTH", default_value_t = false)]
    disable_bluetooth: bool,

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

/// Whether Bluetooth commissioning can actually be offered.
///
/// Three things have to hold, and the answer is reported rather than assumed:
/// the build has the feature, the host is Linux (rs-matter backs BLE nowhere
/// else), and BlueZ is offering an adapter. A client reads this to decide
/// whether to show Bluetooth commissioning at all.
#[cfg(all(feature = "bluetooth", target_os = "linux"))]
fn bluetooth_available(disabled: bool) -> bool {
    use rs_matter_server::matter::ble::BleAdapter;

    if disabled {
        log::info!("Bluetooth commissioning is turned off by --disable-bluetooth");
        return false;
    }

    let probe = async_std::task::block_on(async {
        match BleAdapter::open(None).await {
            Ok(adapter) => adapter.adapter_present().await,
            Err(error) => {
                log::info!("Bluetooth is unavailable: {}", error.details);
                false
            }
        }
    });

    if probe {
        log::info!("Bluetooth commissioning is available");
    } else {
        log::info!("No Bluetooth adapter is available; commissioning will use the network only");
    }

    probe
}

/// Without the feature, or off Linux, there is nothing to probe.
#[cfg(not(all(feature = "bluetooth", target_os = "linux")))]
fn bluetooth_available(_disabled: bool) -> bool {
    false
}

/// Install the logger.
///
/// `set_loglevel` changes the level at runtime through `log::set_max_level`,
/// which is a ceiling over whatever filter the logger was built with — so a
/// logger built at `info` can never be widened to `debug`, however the client
/// asks. Building it at `trace` and then lowering the ceiling to the requested
/// level makes the runtime control work in both directions, at no cost:
/// suppressed records are rejected by the ceiling before their arguments are
/// evaluated.
///
/// `RUST_LOG` is honoured when set and left alone, since per-module filters
/// are the reason to reach for it and clamping them would defeat that.
fn init_logging(level: &str) {
    let mut builder = env_logger::Builder::new();

    match std::env::var("RUST_LOG") {
        Ok(spec) => {
            builder.parse_filters(&spec);
            builder.init();
        }
        Err(_) => {
            builder.filter_level(log::LevelFilter::Trace);
            builder.init();
            log::set_max_level(rs_matter_server::api::server_info::level_filter(level));
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.health_check {
        return health_check(args.listen);
    }

    init_logging(&args.log_level);

    // Before anything writes: the storage directory holds the fabric's signing
    // material and the Wi-Fi password in cleartext.
    rs_matter_server::storage::private::restrict_new_files();

    log::info!("rs-matter-server starting on {}", args.listen);
    log::info!("Storage path: {}", args.storage_path);

    rs_matter_server::storage::private::tighten_existing(std::path::Path::new(&args.storage_path));

    // Read the source before touching our own storage: a source that cannot be
    // imported must not leave a freshly created fabric of our own behind,
    // because that is the state a retry would then refuse to import into.
    let import = match &args.import_matterjs {
        Some(source) => Some(rs_matter_server::migrate::read(
            source,
            args.import_matterjs_namespace.as_deref(),
        )?),
        None => None,
    };

    if args.import_matterjs_dry_run {
        match &import {
            Some(import) => print!("{}", import.summary()),
            None => anyhow::bail!("--import-matterjs-dry-run needs --import-matterjs"),
        }
        return Ok(());
    }

    let controller = init_controller_with_import(
        &args.storage_path,
        &FabricConfig {
            port: args.matter_port,
            ..FabricConfig::default()
        },
        import.as_ref().map(|import| &import.fabric),
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "Could not initialize the Matter controller with storage at '{}': {:?}. \
             That directory holds the fabric and must be writable by the user the \
             server runs as (uid 65532 in the container image).",
            args.storage_path,
            e
        )
    })?;

    let storage = std::path::Path::new(&args.storage_path);

    // The node list and the settings belong to the fabric, so they are written
    // only when the fabric itself was adopted.
    if controller.origin == FabricOrigin::Imported {
        if let Some(import) = &import {
            rs_matter_server::migrate::apply_state(import, storage)?;
        }
    }
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
            bluetooth_enabled: bluetooth_available(args.disable_bluetooth),
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
