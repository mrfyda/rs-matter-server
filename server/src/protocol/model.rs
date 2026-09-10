//! Wire models.
//!
//! These structs are the contract with the client: field names and optionality
//! mirror `@matter-server/ws-client`'s TypeScript models exactly. Fields that
//! the reference marks optional are `Option` + `skip_serializing_if`, so an
//! absent value is omitted rather than sent as `null` — clients distinguish the
//! two.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Node IDs at or above this value are reserved for imported test nodes.
pub const TEST_NODE_START: u64 = 0xFFFF_FFFE_0000_0000;

pub const SCHEMA_VERSION: u64 = 13;
pub const MIN_SUPPORTED_SCHEMA_VERSION: u64 = 11;

/// Attribute values keyed by `"endpoint/cluster/attribute"`.
pub type AttributesData = BTreeMap<String, Value>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub fabric_id: u64,
    pub compressed_fabric_id: u64,
    /// OHF extension; absent in the Python Matter Server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fabric_index: Option<u8>,
    pub schema_version: u64,
    pub min_supported_schema_version: u64,
    pub sdk_version: String,
    pub wifi_credentials_set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi_ssid: Option<String>,
    pub thread_credentials_set: bool,
    pub bluetooth_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ble_proxy_enabled: Option<bool>,
    /// The controller's own operational (CASE) node id. OHF extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller_node_id: Option<u64>,
}

/// A commissioned node, in the exact shape the client expects.
///
/// `attribute_subscriptions` is always empty: like matterjs-server, every
/// attribute is subscribed implicitly, so there is no per-node subscription
/// list to report. The field stays on the wire because the Python server had
/// it and clients still read it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatterNodeData {
    pub node_id: u64,
    pub date_commissioned: String,
    pub last_interview: String,
    pub interview_version: u64,
    pub available: bool,
    pub is_bridge: bool,
    pub attributes: AttributesData,
    pub attribute_subscriptions: Vec<Value>,
    /// Matter specification version, when it could be determined from the
    /// node's BasicInformation cluster. OHF extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matter_version: Option<String>,
}

impl MatterNodeData {
    pub fn new(node_id: u64, commissioned_at: String) -> Self {
        Self {
            node_id,
            last_interview: commissioned_at.clone(),
            date_commissioned: commissioned_at,
            interview_version: 0,
            available: true,
            is_bridge: false,
            attributes: AttributesData::new(),
            attribute_subscriptions: Vec::new(),
            matter_version: None,
        }
    }

    pub fn is_test_node(&self) -> bool {
        self.node_id >= TEST_NODE_START
    }

    /// String attribute lookup on the BasicInformation cluster of endpoint 0.
    pub fn basic_info_string(&self, attribute: u32) -> Option<&str> {
        self.attributes
            .get(&format!("0/40/{}", attribute))
            .and_then(Value::as_str)
    }

    pub fn vendor_name(&self) -> Option<&str> {
        self.basic_info_string(1)
    }

    pub fn product_name(&self) -> Option<&str> {
        self.basic_info_string(3)
    }

    /// The set of endpoint ids the node reported during its interview.
    pub fn endpoints(&self) -> Vec<u16> {
        let mut endpoints: Vec<u16> = self
            .attributes
            .keys()
            .filter_map(|path| path.split('/').next()?.parse::<u16>().ok())
            .collect();
        endpoints.sort_unstable();
        endpoints.dedup();
        endpoints
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommissionableNodeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_discriminator: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commissioning_mode: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_type: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_hint: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mrp_retry_interval_idle: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mrp_retry_interval_active: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tcp: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotating_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MatterFabricData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fabric_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fabric_index: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fabric_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommissioningParameters {
    pub setup_pin_code: u32,
    pub setup_manual_code: String,
    pub setup_qr_code: String,
}

/// Result of `set_acl_entry` / `set_node_binding`.
///
/// Note the snake_case shape here: `write_attribute` reports the *same*
/// information with capitalised `Path`/`Status` keys. That asymmetry is in the
/// reference server and clients depend on it, so it is reproduced rather than
/// unified.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttributeWriteResult {
    pub path: AttributeWritePath,
    pub status: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttributeWritePath {
    pub endpoint_id: u16,
    pub cluster_id: u32,
    pub attribute_id: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatterNodeEvent {
    pub node_id: u64,
    pub endpoint_id: u16,
    pub cluster_id: u32,
    pub event_id: u32,
    pub event_number: u64,
    pub priority: u8,
    pub timestamp: u64,
    pub timestamp_type: u8,
    pub data: Value,
}

/// One Thread network's diagnostics, as `get_thread_diagnostics` answers and
/// `thread_diagnostics_updated` streams.
///
/// A batch is a snapshot: collection runs over a window and publishes what it
/// has, so `partial_reason` says why an answer is not the whole mesh — and its
/// absence is what makes a batch complete, and cacheable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadDiagnosticsBatch {
    /// 16-char uppercase hex of the network's extended PAN id.
    #[serde(rename = "extPanIdHex")]
    pub ext_pan_id_hex: String,
    #[serde(rename = "networkName")]
    pub network_name: String,
    /// Epoch milliseconds the batch was assembled.
    #[serde(rename = "collectedAt")]
    pub collected_at: i64,
    /// What produced it. `none` when nothing was attempted — no credentials
    /// to speak MeshCoP with, and no border router offering the REST API.
    pub source: ThreadDiagnosticsSource,
    pub nodes: Vec<ThreadDiagnosticsNode>,
    #[serde(rename = "partialReason", skip_serializing_if = "Option::is_none")]
    pub partial_reason: Option<ThreadDiagnosticsPartial>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadDiagnosticsSource {
    Meshcop,
    OtbrRest,
    None,
}

/// Why a batch is not the whole mesh. Absent once it is.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadDiagnosticsPartial {
    PetitionRejected,
    DtlsFailed,
    BorderRouterUnreachable,
    /// No way in: no stored dataset to authenticate MeshCoP with, and no
    /// border router offering the REST API.
    NoCredentials,
    NoSource,
    RestUnreachable,
    RestProtocol,
    Timeout,
    /// A collection is still running; more nodes may follow.
    InProgress,
    MeshcopNoResponsesYet,
    RestNoResponsesYet,
}

/// One node's diagnostics, assembled from the TLVs it returned.
///
/// Every field is optional because a node answers with the TLVs it supports.
/// The names are the reference's, which are the Thread specification's.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThreadDiagnosticsNode {
    /// 16-char uppercase hex of the 64-bit Thread MAC address.
    #[serde(rename = "extMacAddress", skip_serializing_if = "Option::is_none")]
    pub ext_mac_address: Option<String>,
    /// Short 16-bit routing locator within the mesh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rloc16: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ThreadMode>,
    /// Polling/child timeout in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connectivity: Option<ThreadConnectivity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route64: Option<ThreadRoute64>,
    #[serde(rename = "leaderData", skip_serializing_if = "Option::is_none")]
    pub leader_data: Option<ThreadLeaderData>,
    /// Hex-encoded raw Network Data blob.
    #[serde(rename = "networkData", skip_serializing_if = "Option::is_none")]
    pub network_data: Option<String>,
    /// Each 16-byte address as uppercase hex.
    #[serde(rename = "ipv6Addresses", skip_serializing_if = "Option::is_none")]
    pub ipv6_addresses: Option<Vec<String>>,
    #[serde(rename = "macCounters", skip_serializing_if = "Option::is_none")]
    pub mac_counters: Option<ThreadMacCounters>,
    #[serde(rename = "childTable", skip_serializing_if = "Option::is_none")]
    pub child_table: Option<Vec<ThreadChildTableEntry>>,
    #[serde(rename = "channelPages", skip_serializing_if = "Option::is_none")]
    pub channel_pages: Option<Vec<u8>>,
    #[serde(rename = "maxChildTimeout", skip_serializing_if = "Option::is_none")]
    pub max_child_timeout: Option<u32>,
    /// 16-char uppercase hex EUI-64.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eui64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u16>,
    #[serde(rename = "vendorName", skip_serializing_if = "Option::is_none")]
    pub vendor_name: Option<String>,
    #[serde(rename = "vendorModel", skip_serializing_if = "Option::is_none")]
    pub vendor_model: Option<String>,
    #[serde(rename = "vendorSwVersion", skip_serializing_if = "Option::is_none")]
    pub vendor_sw_version: Option<String>,
    #[serde(rename = "threadStackVersion", skip_serializing_if = "Option::is_none")]
    pub thread_stack_version: Option<String>,
    #[serde(rename = "vendorAppUrl", skip_serializing_if = "Option::is_none")]
    pub vendor_app_url: Option<String>,
    #[serde(rename = "mleCounters", skip_serializing_if = "Option::is_none")]
    pub mle_counters: Option<ThreadMleCounters>,
    /// Battery level as a percentage, and supply voltage in millivolts.
    #[serde(rename = "batteryLevel", skip_serializing_if = "Option::is_none")]
    pub battery_level: Option<u8>,
    #[serde(rename = "supplyVoltage", skip_serializing_if = "Option::is_none")]
    pub supply_voltage: Option<u16>,
    /// TLVs this decoder does not model, kept verbatim as uppercase hex.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown: Option<Vec<ThreadUnknownTlv>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadUnknownTlv {
    #[serde(rename = "type")]
    pub tlv_type: u8,
    pub value: String,
}

/// MODE TLV: what a node is capable of.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ThreadMode {
    /// Radio stays on when idle — a non-sleepy device.
    #[serde(rename = "rxOnWhenIdle")]
    pub rx_on_when_idle: bool,
    /// Full Thread Device (router-eligible) rather than a minimal one.
    pub ftd: bool,
    /// Wants the full Network Data rather than the stable subset.
    #[serde(rename = "fullNetworkData")]
    pub full_network_data: bool,
}

/// CONNECTIVITY TLV: a node's view of its links.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ThreadConnectivity {
    /// Suitability as a parent: -1 low, 0 medium, 1 high.
    #[serde(rename = "parentPriority")]
    pub parent_priority: i8,
    /// Neighbours at each link quality, 3 being best.
    #[serde(rename = "linkQuality3")]
    pub link_quality3: u8,
    #[serde(rename = "linkQuality2")]
    pub link_quality2: u8,
    #[serde(rename = "linkQuality1")]
    pub link_quality1: u8,
    #[serde(rename = "leaderCost")]
    pub leader_cost: u8,
    #[serde(rename = "idSequence")]
    pub id_sequence: u8,
    #[serde(rename = "activeRouters")]
    pub active_routers: u8,
    /// What a parent reserves for a sleepy child.
    #[serde(rename = "sedBufferSize")]
    pub sed_buffer_size: u16,
    #[serde(rename = "sedDatagramCount")]
    pub sed_datagram_count: u8,
}

/// One row of a ROUTE64 TLV.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ThreadRoute64Entry {
    #[serde(rename = "routerId")]
    pub router_id: u8,
    #[serde(rename = "linkQualityIn")]
    pub link_quality_in: u8,
    #[serde(rename = "linkQualityOut")]
    pub link_quality_out: u8,
    #[serde(rename = "routeCost")]
    pub route_cost: u8,
}

/// ROUTE64 TLV: the node's routing table to other routers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadRoute64 {
    #[serde(rename = "idSequence")]
    pub id_sequence: u8,
    pub entries: Vec<ThreadRoute64Entry>,
}

/// LEADER_DATA TLV: who leads this partition.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ThreadLeaderData {
    /// Changes when a partition splits or merges.
    #[serde(rename = "partitionId")]
    pub partition_id: u32,
    pub weighting: u8,
    #[serde(rename = "dataVersion")]
    pub data_version: u8,
    #[serde(rename = "stableDataVersion")]
    pub stable_data_version: u8,
    #[serde(rename = "leaderRouterId")]
    pub leader_router_id: u8,
}

/// MAC_COUNTERS TLV: packet counters since the last reset.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ThreadMacCounters {
    #[serde(rename = "ifInUnknownProtos")]
    pub if_in_unknown_protos: u32,
    #[serde(rename = "ifInErrors")]
    pub if_in_errors: u32,
    #[serde(rename = "ifOutErrors")]
    pub if_out_errors: u32,
    #[serde(rename = "ifInUcastPkts")]
    pub if_in_ucast_pkts: u32,
    #[serde(rename = "ifInBroadcastPkts")]
    pub if_in_broadcast_pkts: u32,
    #[serde(rename = "ifInDiscards")]
    pub if_in_discards: u32,
    #[serde(rename = "ifOutUcastPkts")]
    pub if_out_ucast_pkts: u32,
    #[serde(rename = "ifOutBroadcastPkts")]
    pub if_out_broadcast_pkts: u32,
    #[serde(rename = "ifOutDiscards")]
    pub if_out_discards: u32,
}

/// One child attached to a router, from its CHILD_TABLE TLV.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ThreadChildTableEntry {
    /// The timeout as its 2^exponent form and in seconds.
    #[serde(rename = "timeoutExponent")]
    pub timeout_exponent: u8,
    #[serde(rename = "timeoutSeconds")]
    pub timeout_seconds: u32,
    #[serde(rename = "incomingLinkQuality")]
    pub incoming_link_quality: u8,
    #[serde(rename = "childId")]
    pub child_id: u16,
    pub mode: ThreadMode,
}

/// MLE_COUNTERS TLV: how often a node has changed role, and for how long.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ThreadMleCounters {
    #[serde(rename = "disabledRole")]
    pub disabled_role: u16,
    #[serde(rename = "detachedRole")]
    pub detached_role: u16,
    #[serde(rename = "childRole")]
    pub child_role: u16,
    #[serde(rename = "routerRole")]
    pub router_role: u16,
    #[serde(rename = "leaderRole")]
    pub leader_role: u16,
    #[serde(rename = "attachAttempts")]
    pub attach_attempts: u16,
    #[serde(rename = "partitionIdChanges")]
    pub partition_id_changes: u16,
    #[serde(rename = "betterPartitionAttachAttempts")]
    pub better_partition_attach_attempts: u16,
    #[serde(rename = "parentChanges")]
    pub parent_changes: u16,
    /// Cumulative milliseconds, which is why these are 64-bit.
    #[serde(rename = "trackedTime")]
    pub tracked_time: u64,
    #[serde(rename = "disabledTime")]
    pub disabled_time: u64,
    #[serde(rename = "detachedTime")]
    pub detached_time: u64,
    #[serde(rename = "childTime")]
    pub child_time: u64,
    #[serde(rename = "routerTime")]
    pub router_time: u64,
    #[serde(rename = "leaderTime")]
    pub leader_time: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateSource {
    MainNetDcl,
    TestNetDcl,
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatterSoftwareVersion {
    pub vid: u16,
    pub pid: u16,
    pub software_version: u64,
    pub software_version_string: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_information: Option<String>,
    pub min_applicable_software_version: u64,
    pub max_applicable_software_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_notes_url: Option<String>,
    pub update_source: UpdateSource,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OtaUploadTicket {
    pub upload_id: String,
    pub expires_in: u64,
    pub max_size: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum IcdOperatingMode {
    #[serde(rename = "SIT")]
    Sit,
    #[serde(rename = "LIT")]
    Lit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IcdStateData {
    pub supported: bool,
    pub lit_supported: bool,
    pub registered: bool,
    pub operating_mode: Option<IcdOperatingMode>,
    pub awake: Option<bool>,
    pub available: Option<bool>,
    /// Epoch milliseconds of the next expected check-in.
    pub next_expected_checkin: Option<u64>,
}

impl IcdStateData {
    /// The response for a node with no ICD Management cluster.
    pub fn unsupported() -> Self {
        Self {
            supported: false,
            lit_supported: false,
            registered: false,
            operating_mode: None,
            awake: None,
            available: None,
            next_expected_checkin: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AllCredentialsSummary {
    pub wifi: Vec<WifiCredentialSummary>,
    pub thread: Vec<ThreadCredentialSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WifiCredentialSummary {
    pub id: String,
    pub ssid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadCredentialSummary {
    pub id: String,
    #[serde(rename = "networkName", skip_serializing_if = "Option::is_none")]
    pub network_name: Option<String>,
    #[serde(rename = "extPanId", skip_serializing_if = "Option::is_none")]
    pub ext_pan_id: Option<String>,
}

/// `ping_node` reports reachability per resolved address.
pub type NodePingResult = BTreeMap<String, bool>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLevelResponse {
    pub console_loglevel: String,
    pub file_loglevel: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NetworkTopology {
    pub collected_at: i64,
    pub nodes: Vec<NetworkTopologyNode>,
    pub connections: Vec<NetworkTopologyConnection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkTopologyNode {
    pub id: String,
    pub kind: String,
    pub network_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_bridge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rloc16: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_pan_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bssid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkTopologyConnection {
    pub source: String,
    pub target: String,
    pub network: String,
    pub strength: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_to_target: Option<TopologyDirectionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_to_source: Option<TopologyDirectionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_route_table: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_cost: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TopologyDirectionInfo {
    pub strength: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lqi: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi: Option<i16>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_data_omits_absent_optional_fields() {
        let node = MatterNodeData::new(7, "2026-01-01T00:00:00.000Z".into());
        let wire = serde_json::to_value(&node).unwrap();
        assert!(wire.get("matter_version").is_none());
        assert_eq!(wire["attribute_subscriptions"], json!([]));
        assert_eq!(wire["last_interview"], wire["date_commissioned"]);
    }

    #[test]
    fn endpoints_are_derived_from_attribute_paths() {
        let mut node = MatterNodeData::new(1, "now".into());
        node.attributes.insert("0/40/1".into(), json!("ACME"));
        node.attributes.insert("1/6/0".into(), json!(true));
        node.attributes.insert("1/29/0".into(), json!([]));
        assert_eq!(node.endpoints(), vec![0, 1]);
    }

    #[test]
    fn test_node_range_is_recognised() {
        assert!(MatterNodeData::new(TEST_NODE_START, "now".into()).is_test_node());
        assert!(!MatterNodeData::new(TEST_NODE_START - 1, "now".into()).is_test_node());
    }

    #[test]
    fn update_source_uses_kebab_case_on_the_wire() {
        assert_eq!(
            serde_json::to_value(UpdateSource::MainNetDcl).unwrap(),
            json!("main-net-dcl")
        );
    }

    #[test]
    fn icd_operating_mode_serialises_uppercase() {
        assert_eq!(
            serde_json::to_value(IcdOperatingMode::Lit).unwrap(),
            json!("LIT")
        );
    }
}
