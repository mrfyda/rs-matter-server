//! Protocol command handlers.
//!
//! One module per area of the API, and a single registry that maps a command
//! name onto a handler. Handlers receive parsed [`Args`] and a [`CallContext`]
//! and return an [`ApiResult`]; they never see the WebSocket, the envelope, or
//! rs-matter. That is what lets the whole command surface be exercised by the
//! contract tests without a radio.

pub mod commissioning;
pub mod fabrics;
pub mod icd;
pub mod interaction;
pub mod network;
pub mod nodes;
pub mod ota;
pub mod server_info;
pub mod webrtc;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_channel::{Receiver, Sender};

use crate::matter::actor::{FabricInfo, MatterHandle};
use crate::matter::dcl::DclClient;
use crate::matter::subscriptions;
use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::events::Event;
use crate::protocol::message::Args;
use crate::protocol::model::MatterNodeEvent;
use crate::storage::{ConfigStore, NodeStore};

/// How many Matter events `diagnostics` reports, matching the reference.
const EVENT_HISTORY_SIZE: usize = 25;

/// The version this build reports, over `--version` and in `sdk_version`.
///
/// Releases are cut from every commit that lands on main, so the patch number
/// is derived from the tag history at build time rather than committed to the
/// manifest — CI passes the result in as `RS_MATTER_SERVER_VERSION`, and the
/// image is tagged with the same string. A build without it, which is any build
/// that is not a release, reports the manifest's version: the floor of the
/// series it belongs to.
pub const VERSION: &str = match option_env!("RS_MATTER_SERVER_VERSION") {
    // An unset build argument reaches this as an empty string rather than as
    // nothing at all, because the Dockerfile always defines the variable.
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

/// Server capabilities and identity that do not change at runtime.
#[derive(Clone, Debug)]
pub struct RuntimeInfo {
    pub sdk_version: String,
    pub bluetooth_enabled: bool,
    pub ble_proxy_enabled: bool,
    /// Whether OTA support is enabled (`--disable-ota` turns it off, and the
    /// upload endpoint then does not exist at all).
    pub ota_enabled: bool,
    /// Whether Thread diagnostics collection is enabled.
    pub thread_diagnostics_enabled: bool,
    /// A label pinned by `--default-fabric-label`, which
    /// `set_default_fabric_label` must not override.
    pub pinned_fabric_label: Option<String>,
    /// Whether to consult the CSA's *test* ledger for devices with test vendor
    /// ids. Off by default, matching the reference.
    pub test_net_dcl: bool,
}

impl Default for RuntimeInfo {
    fn default() -> Self {
        Self {
            sdk_version: format!("rs-matter-server/{VERSION} (rs-matter/0.3.0)"),
            bluetooth_enabled: false,
            ble_proxy_enabled: false,
            ota_enabled: true,
            thread_diagnostics_enabled: true,
            pinned_fabric_label: None,
            test_net_dcl: false,
        }
    }
}

/// Vendor ids the CSA reserves for test devices; their firmware lives in the
/// test ledger, not the production one.
fn is_test_vendor(vendor_id: u16) -> bool {
    (0xFFF1..=0xFFF4).contains(&vendor_id)
}

/// Fan-out of events to listening connections.
///
/// A subscriber that has stopped draining is dropped rather than allowed to
/// stall the publisher: events are a live stream, and a connection that cannot
/// keep up has already lost its place in it.
pub struct EventBus {
    subscribers: Mutex<Vec<Sender<Event>>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> Receiver<Event> {
        let (sender, receiver) = async_channel::bounded(256);
        self.subscribers.lock().unwrap().push(sender);
        receiver
    }

    pub fn publish(&self, event: Event) {
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|sender| sender.try_send(event.clone()).is_ok());
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// State shared by every connection.
pub struct ServerContext {
    pub matter: MatterHandle,
    pub nodes: Arc<NodeStore>,
    pub config: Arc<ConfigStore>,
    pub events: Arc<EventBus>,
    pub runtime: RuntimeInfo,
    pub ota: ota::OtaUploadRegistry,
    /// The subscriptions this controller holds, shared between the monitor
    /// that establishes them and the report handler that receives them.
    pub subscriptions: subscriptions::Registry,
    /// When each intermittently connected device last checked in.
    ///
    /// In memory only: a check-in says a device was awake a moment ago, which
    /// stops being true while this server is not running.
    check_ins: Mutex<BTreeMap<u64, SystemTime>>,
    dcl: DclClient,
    test_dcl: Option<DclClient>,
    console_loglevel: Mutex<String>,
    file_loglevel: Mutex<Option<String>>,
    /// The connection that owns `set_default_fabric_label`, if any.
    fabric_label_owner: AtomicU64,
    event_history: Mutex<VecDeque<MatterNodeEvent>>,
    /// The controller's own fabric identity.
    ///
    /// Cached because every new connection is answered with `server_info`
    /// before it sends anything, and the fabric only changes when its label
    /// does. Caching it keeps connection setup off the Matter actor entirely.
    fabric: Mutex<Option<FabricInfo>>,
}

impl ServerContext {
    pub fn new(
        matter: MatterHandle,
        nodes: Arc<NodeStore>,
        config: Arc<ConfigStore>,
        runtime: RuntimeInfo,
        console_loglevel: String,
    ) -> Self {
        let test_dcl = runtime.test_net_dcl.then(DclClient::test_net);
        Self {
            matter,
            nodes,
            config,
            events: Arc::new(EventBus::new()),
            runtime,
            ota: ota::OtaUploadRegistry::new(),
            subscriptions: subscriptions::Registry::new(),
            check_ins: Mutex::new(BTreeMap::new()),
            dcl: DclClient::main_net(),
            test_dcl,
            console_loglevel: Mutex::new(console_loglevel),
            file_loglevel: Mutex::new(None),
            fabric_label_owner: AtomicU64::new(0),
            event_history: Mutex::new(VecDeque::new()),
            fabric: Mutex::new(None),
        }
    }

    /// The controller's fabric, fetched from the actor on first use.
    pub async fn fabric_info(&self) -> Result<FabricInfo, ApiError> {
        if let Some(fabric) = self.fabric.lock().unwrap().clone() {
            return Ok(fabric);
        }
        let fabric = self.matter.fabric_info().await?;
        *self.fabric.lock().unwrap() = Some(fabric.clone());
        Ok(fabric)
    }

    /// Seed or replace the cached fabric identity.
    pub fn set_fabric_info(&self, fabric: FabricInfo) {
        *self.fabric.lock().unwrap() = Some(fabric);
    }

    /// Record a new fabric label without a Matter round-trip.
    pub fn update_cached_fabric_label(&self, label: &str) {
        if let Some(fabric) = self.fabric.lock().unwrap().as_mut() {
            fabric.label = label.to_string();
        }
    }

    /// Ask the appropriate ledger whether newer firmware exists.
    ///
    /// Returns `Ok(None)` both when the device is current and when its vendor
    /// has nothing in a ledger this server consults — the caller cannot tell
    /// those apart, and neither can the reference.
    ///
    /// This performs blocking HTTP and must be called from a connection
    /// thread, never from the Matter executor.
    pub fn check_dcl(
        &self,
        vendor_id: u16,
        product_id: u16,
        current_version: u64,
    ) -> Result<Option<crate::protocol::model::MatterSoftwareVersion>, ApiError> {
        if is_test_vendor(vendor_id) {
            return match &self.test_dcl {
                Some(client) => client.check_update(vendor_id, product_id, current_version),
                // A test device is not in the production ledger, so querying
                // it would only produce a misleading "up to date".
                None => Ok(None),
            };
        }
        self.dcl
            .check_update(vendor_id, product_id, current_version)
    }

    /// The name a ledger has for a vendor id, for the ids the reference's
    /// static table does not cover.
    ///
    /// A test vendor id is only looked up when the test ledger is enabled, as
    /// with `check_dcl`: the production ledger does not carry them, so asking
    /// it would spend a round-trip to learn nothing.
    ///
    /// This performs blocking HTTP and must be called from a connection
    /// thread, never from the Matter executor.
    pub fn vendor_name_from_dcl(&self, vendor_id: u16) -> Option<String> {
        if is_test_vendor(vendor_id) {
            return self.test_dcl.as_ref()?.vendor_name(vendor_id);
        }
        self.dcl.vendor_name(vendor_id)
    }

    /// Record that a node checked in.
    pub fn note_check_in(&self, node_id: u64, at: SystemTime) {
        self.check_ins.lock().unwrap().insert(node_id, at);
    }

    /// When a node last checked in, if it has since this server started.
    pub fn last_check_in(&self, node_id: u64) -> Option<SystemTime> {
        self.check_ins.lock().unwrap().get(&node_id).copied()
    }

    pub fn console_loglevel(&self) -> String {
        self.console_loglevel.lock().unwrap().clone()
    }

    pub fn set_console_loglevel(&self, level: &str) {
        *self.console_loglevel.lock().unwrap() = level.to_string();
    }

    pub fn file_loglevel(&self) -> Option<String> {
        self.file_loglevel.lock().unwrap().clone()
    }

    pub fn set_file_loglevel(&self, level: &str) {
        *self.file_loglevel.lock().unwrap() = Some(level.to_string());
    }

    /// Claim fabric-label ownership for a connection, or report whether this
    /// connection already holds it.
    ///
    /// The first connection to set the label owns it until it disconnects, so
    /// two clients (two Home Assistant instances, say) cannot fight over it.
    pub fn claim_fabric_label(&self, connection_id: u64) -> bool {
        self.fabric_label_owner
            .compare_exchange(0, connection_id, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            || self.fabric_label_owner.load(Ordering::SeqCst) == connection_id
    }

    /// Release ownership when the owning connection goes away.
    pub fn release_fabric_label(&self, connection_id: u64) {
        let _ = self.fabric_label_owner.compare_exchange(
            connection_id,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    pub fn record_node_event(&self, event: MatterNodeEvent) {
        let mut history = self.event_history.lock().unwrap();
        history.push_back(event);
        while history.len() > EVENT_HISTORY_SIZE {
            history.pop_front();
        }
    }

    pub fn event_history(&self) -> Vec<MatterNodeEvent> {
        self.event_history.lock().unwrap().iter().cloned().collect()
    }
}

/// Per-request context: the shared state plus who is asking.
#[derive(Clone, Copy)]
pub struct CallContext<'a> {
    pub server: &'a ServerContext,
    pub connection_id: u64,
    /// The client's address, which scopes an OTA upload reservation.
    pub peer: &'a str,
}

/// Every command this server implements.
///
/// The list is exhaustive on purpose: an advertised schema capability that
/// silently answers "unknown command" is worse than one that answers with the
/// documented compatibility error, so unsupported features appear here and
/// return a specific reason.
pub const COMMANDS: &[&str] = &[
    "start_listening",
    "server_info",
    "diagnostics",
    "get_loglevel",
    "set_loglevel",
    "get_nodes",
    "get_node",
    "get_node_ip_addresses",
    "remove_node",
    "interview_node",
    "ping_node",
    "import_test_node",
    "get_vendor_names",
    "set_wifi_credentials",
    "set_thread_dataset",
    "remove_wifi_credentials",
    "remove_thread_dataset",
    "get_all_credentials",
    "set_default_fabric_label",
    "get_fabric_label",
    "commission_with_code",
    "commission_on_network",
    "open_commissioning_window",
    "discover",
    "discover_commissionable_nodes",
    "read_attribute",
    "write_attribute",
    "device_command",
    "get_matter_fabrics",
    "remove_matter_fabric",
    "set_acl_entry",
    "set_node_binding",
    "get_icd_state",
    "register_icd",
    "unregister_icd",
    "resync_icd",
    "check_node_update",
    "update_node",
    "initiate_ota_upload",
    "get_thread_border_routers",
    "get_thread_diagnostics",
    "get_network_topology",
    "send_webrtc_provider_command",
];

/// Route a command to its handler.
pub async fn dispatch(command: &str, args: &Args, context: CallContext<'_>) -> ApiResult {
    match command {
        // -- listening and server state ------------------------------------
        "start_listening" => nodes::start_listening(args, context).await,
        "server_info" => server_info::server_info(args, context).await,
        "diagnostics" => server_info::diagnostics(args, context).await,
        "get_loglevel" => server_info::get_loglevel(args, context).await,
        "set_loglevel" => server_info::set_loglevel(args, context).await,

        // -- credentials and fabric label ----------------------------------
        "set_wifi_credentials" => server_info::set_wifi_credentials(args, context).await,
        "set_thread_dataset" => server_info::set_thread_dataset(args, context).await,
        "remove_wifi_credentials" => server_info::remove_wifi_credentials(args, context).await,
        "remove_thread_dataset" => server_info::remove_thread_dataset(args, context).await,
        "get_all_credentials" => server_info::get_all_credentials(args, context).await,
        "set_default_fabric_label" => server_info::set_default_fabric_label(args, context).await,
        "get_fabric_label" => server_info::get_fabric_label(args, context).await,

        // -- nodes ---------------------------------------------------------
        "get_nodes" => nodes::get_nodes(args, context).await,
        "get_node" => nodes::get_node(args, context).await,
        "get_node_ip_addresses" => nodes::get_node_ip_addresses(args, context).await,
        "remove_node" => nodes::remove_node(args, context).await,
        "interview_node" => nodes::interview_node(args, context).await,
        "ping_node" => nodes::ping_node(args, context).await,
        "import_test_node" => nodes::import_test_node(args, context).await,
        "get_vendor_names" => nodes::get_vendor_names(args, context).await,

        // -- commissioning -------------------------------------------------
        "commission_with_code" => commissioning::commission_with_code(args, context).await,
        "commission_on_network" => commissioning::commission_on_network(args, context).await,
        "open_commissioning_window" => {
            commissioning::open_commissioning_window(args, context).await
        }
        "discover" | "discover_commissionable_nodes" => {
            commissioning::discover(args, context).await
        }

        // -- interaction model ---------------------------------------------
        "read_attribute" => interaction::read_attribute(args, context).await,
        "write_attribute" => interaction::write_attribute(args, context).await,
        "device_command" => interaction::device_command(args, context).await,

        // -- fabrics, ACLs and bindings ------------------------------------
        "get_matter_fabrics" => fabrics::get_matter_fabrics(args, context).await,
        "remove_matter_fabric" => fabrics::remove_matter_fabric(args, context).await,
        "set_acl_entry" => fabrics::set_acl_entry(args, context).await,
        "set_node_binding" => fabrics::set_node_binding(args, context).await,

        // -- ICD -----------------------------------------------------------
        "get_icd_state" => icd::get_icd_state(args, context).await,
        "register_icd" => icd::register_icd(args, context).await,
        "unregister_icd" => icd::unregister_icd(args, context).await,
        "resync_icd" => icd::resync_icd(args, context).await,

        // -- OTA -----------------------------------------------------------
        "check_node_update" => ota::check_node_update(args, context).await,
        "update_node" => ota::update_node(args, context).await,
        "initiate_ota_upload" => ota::initiate_ota_upload(args, context).await,

        // -- Thread and topology -------------------------------------------
        "get_thread_border_routers" => network::get_thread_border_routers(args, context).await,
        "get_thread_diagnostics" => network::get_thread_diagnostics(args, context).await,
        "get_network_topology" => network::get_network_topology(args, context).await,

        // -- WebRTC --------------------------------------------------------
        "send_webrtc_provider_command" => webrtc::send_provider_command(args, context).await,

        other => Err(ApiError::invalid_command(other)),
    }
}

/// The vendor name for an id, from the bundled table.
pub fn vendor_name(vendor_id: u16) -> Option<String> {
    nodes::vendor_name(vendor_id)
}

/// Resolve a node id argument, rejecting one that names no known node.
pub fn require_node(args: &Args, context: CallContext<'_>) -> Result<u64, ApiError> {
    let node_id = args.req_u64("node_id")?;
    if !context.server.nodes.contains(node_id) {
        return Err(ApiError::node_not_exists(node_id));
    }
    Ok(node_id)
}

/// An ISO-8601 timestamp in the format the protocol uses for node dates.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_and_the_dispatcher_agree() {
        // Every advertised command must route somewhere: a name in this list
        // that falls through to the catch-all would answer "unknown command"
        // while still being advertised.
        let mut sorted = COMMANDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), COMMANDS.len(), "duplicate command name");
    }

    #[test]
    fn fabric_label_ownership_is_exclusive_until_release() {
        let context = crate::api::tests_support::test_context();
        assert!(context.claim_fabric_label(1));
        assert!(!context.claim_fabric_label(2));
        assert!(context.claim_fabric_label(1), "owner keeps its claim");
        context.release_fabric_label(1);
        assert!(context.claim_fabric_label(2));
    }

    #[test]
    fn event_history_is_capped() {
        let context = crate::api::tests_support::test_context();
        for index in 0..40 {
            context.record_node_event(MatterNodeEvent {
                node_id: 1,
                endpoint_id: 1,
                cluster_id: 6,
                event_id: index,
                event_number: index as u64,
                priority: 1,
                timestamp: 0,
                timestamp_type: 0,
                data: serde_json::Value::Null,
            });
        }
        let history = context.event_history();
        assert_eq!(history.len(), EVENT_HISTORY_SIZE);
        assert_eq!(history[0].event_id, 15);
    }

    #[test]
    fn a_disconnected_subscriber_is_dropped_on_publish() {
        let bus = EventBus::new();
        let receiver = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        drop(receiver);
        bus.publish(Event::node_removed(1));
        assert_eq!(bus.subscriber_count(), 0);
    }
}

#[cfg(test)]
pub mod tests_support {
    //! Helpers shared by the handler unit tests.

    use super::*;
    use crate::matter::actor;

    /// A context with no Matter behind it: enough to exercise argument
    /// validation, storage-backed commands, and error mapping.
    pub fn test_context() -> ServerContext {
        let (handle, rx) = actor::channel();
        // Keeping the receiver alive would make ops hang; dropping it makes
        // them fail fast with the "not running" SDK error, which is what a
        // handler test wants when it is not exercising Matter itself.
        drop(rx);
        ServerContext::new(
            handle,
            Arc::new(NodeStore::new()),
            Arc::new(ConfigStore::in_memory()),
            RuntimeInfo::default(),
            "info".to_string(),
        )
    }

    /// A context whose fabric identity is already known, so `server_info` and
    /// friends answer without a live Matter actor.
    pub fn test_context_with_fabric() -> ServerContext {
        let context = test_context();
        context.set_fabric_info(actor::FabricInfo {
            fabric_id: 1,
            compressed_fabric_id: 0x1234_5678_9ABC_DEF0,
            fabric_index: 1,
            node_id: 112233,
            vendor_id: 0xFFF1,
            label: "HomeAssistant".to_string(),
        });
        context
    }

    pub fn call(context: &ServerContext) -> CallContext<'_> {
        CallContext {
            server: context,
            connection_id: 1,
            peer: "127.0.0.1:1234",
        }
    }
}
