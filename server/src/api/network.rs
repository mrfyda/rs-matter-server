//! Thread diagnostics and the derived network topology.
//!
//! The topology graph is built from what the nodes already told us: the Thread
//! and Wi-Fi diagnostics clusters cached from their interviews. `refresh`
//! re-reads those clusters from every reachable node first, which is real radio
//! traffic and therefore only ever user-initiated.
//!
//! Because the graph is derived, it can be kept current for free: `watch_topology`
//! rebuilds it when a node change says it might have moved, and publishes
//! `network_topology_updated` only when the graph is actually different.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::matter::mdns_browser::{self, ServiceInstance};
use crate::protocol::error::ApiResult;
use crate::protocol::events::Event;
use crate::protocol::message::Args;
use crate::protocol::model::{
    MatterNodeData, NetworkTopology, NetworkTopologyConnection, NetworkTopologyNode,
    TopologyDirectionInfo,
};
use crate::protocol::paths::parse_path;

use super::{CallContext, ServerContext};

const THREAD_DIAGNOSTICS_CLUSTER: u32 = 53;
const WIFI_DIAGNOSTICS_CLUSTER: u32 = 54;

/// ThreadNetworkDiagnostics attribute ids.
mod thread_attr {
    pub const ROUTING_ROLE: u32 = 1;
    pub const NETWORK_NAME: u32 = 2;
    pub const EXTENDED_PAN_ID: u32 = 4;
    pub const NEIGHBOR_TABLE: u32 = 7;
    pub const ROUTE_TABLE: u32 = 8;
    pub const EXT_ADDRESS: u32 = 63;
    pub const RLOC16: u32 = 64;
}

/// WiFiNetworkDiagnostics attribute ids.
mod wifi_attr {
    pub const BSSID: u32 = 0;
    pub const RSSI: u32 = 4;
}

/// `NeighborTableStruct` TLV tags.
mod neighbor_tags {
    pub const EXT_ADDRESS: &str = "0";
    pub const RLOC16: &str = "2";
    pub const LQI: &str = "5";
    pub const AVERAGE_RSSI: &str = "6";
}

/// `RouteTableStruct` TLV tags.
mod route_tags {
    pub const EXT_ADDRESS: &str = "0";
    pub const PATH_COST: &str = "4";
    pub const LQI_IN: &str = "5";
    pub const LQI_OUT: &str = "6";
}

/// The MeshCoP service Thread Border Routers advertise.
const MESHCOP_SERVICE: &str = "_meshcop._udp.local";
/// How long a passive browse listens for answers.
const BROWSE_TIMEOUT: Duration = Duration::from_millis(2500);

/// One discovered Border Router, in the shape the reference reports.
#[derive(Clone, Debug, Serialize)]
pub struct BorderRouterEntry {
    /// 16-char uppercase hex of the Border Router's extended (MAC) address.
    #[serde(rename = "extAddressHex", skip_serializing_if = "Option::is_none")]
    pub ext_address_hex: Option<String>,
    /// 16-char uppercase hex of the Thread network's extended PAN id.
    #[serde(rename = "extendedPanIdHex", skip_serializing_if = "Option::is_none")]
    pub extended_pan_id_hex: Option<String>,
    #[serde(rename = "networkName", skip_serializing_if = "Option::is_none")]
    pub network_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub addresses: Vec<String>,
    /// How this entry was learned. Only mDNS discovery is available here.
    pub sources: Vec<String>,
    /// Epoch milliseconds this Border Router was last seen.
    #[serde(rename = "lastSeen")]
    pub last_seen: i64,
    #[serde(rename = "vendorName", skip_serializing_if = "Option::is_none")]
    pub vendor_name: Option<String>,
    #[serde(rename = "modelName", skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

/// Border routers are discovered passively over mDNS `_meshcop._udp`; no
/// credentials are involved.
pub async fn get_thread_border_routers(_args: &Args, context: CallContext<'_>) -> ApiResult {
    if !context.server.runtime.thread_diagnostics_enabled {
        return Ok(json!([]));
    }

    let instances = match mdns_browser::browse(MESHCOP_SERVICE, BROWSE_TIMEOUT).await {
        Ok(instances) => instances,
        Err(error) => {
            log::warn!("Could not browse for Thread Border Routers: {}", error);
            return Ok(json!([]));
        }
    };

    let now = chrono::Utc::now().timestamp_millis();
    let routers: Vec<BorderRouterEntry> = instances
        .iter()
        .map(|instance| border_router(instance, now))
        .collect();
    Ok(serde_json::to_value(routers).unwrap_or(Value::Null))
}

/// Map a MeshCoP mDNS answer onto the wire shape. The TXT keys are the ones
/// the Thread specification defines for this service.
fn border_router(instance: &ServiceInstance, now: i64) -> BorderRouterEntry {
    BorderRouterEntry {
        ext_address_hex: instance.txt_hex("xa"),
        extended_pan_id_hex: instance.txt_hex("xp"),
        network_name: instance.txt_str("nn"),
        // A display label: the trailing dot and the `.local` suffix are noise.
        hostname: instance.host_name.as_ref().map(|host| {
            host.trim_end_matches('.')
                .trim_end_matches(".local")
                .to_string()
        }),
        port: instance.port,
        addresses: instance
            .addresses
            .iter()
            .map(|address| address.to_string())
            .collect(),
        sources: vec!["mdns".to_string()],
        last_seen: now,
        vendor_name: instance.txt_str("vn"),
        model_name: instance.txt_str("mn"),
    }
}

/// Per-network Thread diagnostics.
///
/// Collecting these needs a MeshCoP (CoAP/DTLS) or OTBR REST client against a
/// discovered Border Router, neither of which this build has. The documented
/// "nothing cached" answers are returned: `null` for a single network, an empty
/// list for all of them.
pub async fn get_thread_diagnostics(args: &Args, context: CallContext<'_>) -> ApiResult {
    let _ = context;
    match args.str("ext_pan_id")? {
        Some(ext_pan_id) => {
            if ext_pan_id.len() != 16 || !ext_pan_id.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(crate::protocol::error::ApiError::invalid_args(format!(
                    "Invalid ext_pan_id \"{}\": expected 16 hex characters",
                    ext_pan_id
                )));
            }
            Ok(Value::Null)
        }
        None => Ok(json!([])),
    }
}

/// Publish `network_topology_updated` whenever the graph actually changes.
///
/// The graph is derived from what the nodes report, so it changes when a node
/// is added or removed, when one comes or goes, and when a poll brings back
/// new Thread or Wi-Fi diagnostics. All of those already announce themselves
/// with a node event, so this watches the event stream rather than asking
/// every publisher to remember the topology as well.
///
/// A client that never issued `get_network_topology` is not sent these, so
/// this is one recomputation per burst of node changes and no traffic at all
/// on a server whose clients do not use the graph.
pub async fn watch_topology(context: Arc<ServerContext>) {
    let events = context.events.subscribe();
    watch(context, events).await
}

/// The loop itself, over a subscription the caller made — so a test can
/// subscribe before it publishes anything and not race the watcher.
async fn watch(context: Arc<ServerContext>, events: async_channel::Receiver<Event>) {
    let mut published: Option<Value> = None;

    while let Ok(event) = events.recv().await {
        if !matches!(
            event.name(),
            "node_added" | "node_updated" | "node_removed"
        ) {
            continue;
        }
        // A poll publishes a node event per changed attribute, and an
        // interview publishes one per endpoint. Draining what is already
        // queued collapses that burst into a single rebuild — nothing is lost,
        // because the rebuild reads the store rather than the events.
        while events.try_recv().is_ok() {}

        let topology = build_topology(&context.nodes.all());
        let shape = topology_shape(&topology);
        if published.as_ref() == Some(&shape) {
            continue;
        }
        published = Some(shape);
        context
            .events
            .publish(Event::network_topology_updated(&topology));
    }
}

/// The part of the graph a client cares about seeing change.
///
/// `collected_at` is the time the graph was built, so it differs on every
/// rebuild; comparing it would make every poll look like a change.
fn topology_shape(topology: &NetworkTopology) -> Value {
    json!({
        "nodes": topology.nodes,
        "connections": topology.connections,
    })
}

/// Build the network graph.
pub async fn get_network_topology(args: &Args, context: CallContext<'_>) -> ApiResult {
    if args.bool_or("refresh", false)? {
        refresh_diagnostics(context).await;
    }
    let topology = build_topology(&context.server.nodes.all());
    Ok(serde_json::to_value(topology).unwrap_or(Value::Null))
}

/// Re-read the diagnostics clusters from every node believed to be reachable.
///
/// Failures are skipped rather than propagated: a partial graph built from the
/// nodes that answered is more useful than an error because one sleepy device
/// did not.
async fn refresh_diagnostics(context: CallContext<'_>) {
    for node in context.server.nodes.all_filtered(true) {
        let paths = [THREAD_DIAGNOSTICS_CLUSTER, WIFI_DIAGNOSTICS_CLUSTER]
            .iter()
            .filter_map(|cluster| parse_path(&format!("0/{}/*", cluster)).ok())
            .map(|path| path.to_attr_path())
            .collect::<Vec<_>>();

        match context
            .server
            .matter
            .read_attributes(node.node_id, paths, false)
            .await
        {
            Ok(attributes) => {
                for (path, value) in attributes {
                    context
                        .server
                        .nodes
                        .set_attribute(node.node_id, &path, value);
                }
            }
            Err(error) => log::debug!(
                "Skipping node {} while refreshing the topology: {}",
                node.node_id,
                error
            ),
        }
    }
    let _ = context.server.nodes.save();
}

/// Derive the graph from cached attributes.
pub fn build_topology(nodes: &[MatterNodeData]) -> NetworkTopology {
    let mut graph_nodes: Vec<NetworkTopologyNode> = Vec::new();
    let mut connections: Vec<NetworkTopologyConnection> = Vec::new();

    // Thread nodes are matched to their neighbours by extended address, so
    // index that first.
    let mut by_ext_address: Vec<(String, String)> = Vec::new();
    for node in nodes {
        if let Some(ext) = thread_string(node, thread_attr::EXT_ADDRESS) {
            by_ext_address.push((ext, node.node_id.to_string()));
        }
    }

    for node in nodes {
        let id = node.node_id.to_string();
        let thread_role = thread_attribute(node, thread_attr::ROUTING_ROLE).and_then(Value::as_u64);
        let is_thread =
            thread_role.is_some() || thread_attribute(node, thread_attr::EXT_ADDRESS).is_some();
        let bssid = wifi_attribute(node, wifi_attr::BSSID).and_then(format_bssid);

        let network_type = if is_thread {
            "thread"
        } else if bssid.is_some() {
            "wifi"
        } else {
            "unknown"
        };

        graph_nodes.push(NetworkTopologyNode {
            id: id.clone(),
            kind: "matter".into(),
            network_type: network_type.into(),
            node_id: Some(node.node_id),
            role: if is_thread {
                thread_role.map(routing_role)
            } else if bssid.is_some() {
                Some("station".into())
            } else {
                None
            },
            available: Some(node.available),
            is_bridge: Some(node.is_bridge),
            ext_address: thread_string(node, thread_attr::EXT_ADDRESS),
            rloc16: thread_attribute(node, thread_attr::RLOC16)
                .and_then(Value::as_u64)
                .map(|value| value as u16),
            ext_pan_id: thread_attribute(node, thread_attr::EXTENDED_PAN_ID)
                .and_then(Value::as_u64)
                .map(|value| format!("{:016X}", value)),
            network_name: thread_attribute(node, thread_attr::NETWORK_NAME)
                .and_then(Value::as_str)
                .map(str::to_string),
            ssid: None,
            bssid: bssid.clone(),
            host_name: None,
            vendor_name: node.vendor_name().map(str::to_string),
            model_name: node.product_name().map(str::to_string),
            last_seen: None,
        });

        // Wi-Fi stations hang off a synthetic access-point node, one per BSSID.
        if let Some(bssid) = bssid {
            let ap_id = format!("ap_{}", bssid.replace(':', ""));
            if !graph_nodes.iter().any(|existing| existing.id == ap_id) {
                graph_nodes.push(NetworkTopologyNode {
                    id: ap_id.clone(),
                    kind: "wifi_ap".into(),
                    network_type: "wifi".into(),
                    node_id: None,
                    role: Some("ap".into()),
                    available: None,
                    is_bridge: None,
                    ext_address: None,
                    rloc16: None,
                    ext_pan_id: None,
                    network_name: Some(bssid.clone()),
                    ssid: None,
                    bssid: Some(bssid.clone()),
                    host_name: None,
                    vendor_name: None,
                    model_name: None,
                    last_seen: None,
                });
            }
            let rssi = wifi_attribute(node, wifi_attr::RSSI)
                .and_then(Value::as_i64)
                .map(|value| value as i16);
            let strength = rssi.map(wifi_strength).unwrap_or("unknown").to_string();
            connections.push(NetworkTopologyConnection {
                source: id.clone(),
                target: ap_id,
                network: "wifi".into(),
                strength: strength.clone(),
                source_to_target: Some(TopologyDirectionInfo {
                    strength,
                    lqi: None,
                    rssi,
                }),
                target_to_source: None,
                via_route_table: None,
                path_cost: None,
            });
        }
    }

    // Thread neighbour links.
    for node in nodes {
        let source = node.node_id.to_string();
        let neighbours = thread_attribute(node, thread_attr::NEIGHBOR_TABLE)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for neighbour in neighbours {
            let Some(ext_address) = neighbour
                .get(neighbor_tags::EXT_ADDRESS)
                .and_then(Value::as_str)
                .and_then(decode_ext_address)
            else {
                continue;
            };
            let target = resolve_target(
                &ext_address,
                &by_ext_address,
                &mut graph_nodes,
                neighbour
                    .get(neighbor_tags::RLOC16)
                    .and_then(Value::as_u64)
                    .map(|value| value as u16),
                node,
            );

            let lqi = neighbour
                .get(neighbor_tags::LQI)
                .and_then(Value::as_u64)
                .map(|value| value as u8);
            let rssi = neighbour
                .get(neighbor_tags::AVERAGE_RSSI)
                .and_then(Value::as_i64)
                .map(|value| value as i16);
            let strength = lqi.map(thread_strength).unwrap_or("unknown").to_string();

            merge_connection(
                &mut connections,
                NetworkTopologyConnection {
                    source: source.clone(),
                    target,
                    network: "thread".into(),
                    strength: strength.clone(),
                    source_to_target: Some(TopologyDirectionInfo {
                        strength,
                        lqi,
                        rssi,
                    }),
                    target_to_source: None,
                    via_route_table: None,
                    path_cost: None,
                },
            );
        }

        // Route-table entries fill in links the neighbour table did not report.
        let routes = thread_attribute(node, thread_attr::ROUTE_TABLE)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for route in routes {
            let Some(ext_address) = route
                .get(route_tags::EXT_ADDRESS)
                .and_then(Value::as_str)
                .and_then(decode_ext_address)
            else {
                continue;
            };
            let target =
                resolve_target(&ext_address, &by_ext_address, &mut graph_nodes, None, node);
            if connections
                .iter()
                .any(|existing| links_same_pair(existing, &source, &target))
            {
                continue;
            }
            let lqi = route
                .get(route_tags::LQI_IN)
                .or_else(|| route.get(route_tags::LQI_OUT))
                .and_then(Value::as_u64)
                .map(|value| value as u8);
            connections.push(NetworkTopologyConnection {
                source: source.clone(),
                target,
                network: "thread".into(),
                strength: lqi.map(thread_strength).unwrap_or("unknown").to_string(),
                source_to_target: None,
                target_to_source: None,
                via_route_table: Some(true),
                path_cost: route
                    .get(route_tags::PATH_COST)
                    .and_then(Value::as_u64)
                    .map(|value| value as u32),
            });
        }
    }

    NetworkTopology {
        collected_at: chrono::Utc::now().timestamp_millis(),
        nodes: graph_nodes,
        connections,
    }
}

/// Find the graph id for a Thread extended address, inventing a
/// `thread_unknown` node for a neighbour that is not on this fabric.
fn resolve_target(
    ext_address: &str,
    by_ext_address: &[(String, String)],
    graph_nodes: &mut Vec<NetworkTopologyNode>,
    rloc16: Option<u16>,
    observer: &MatterNodeData,
) -> String {
    if let Some((_, node_id)) = by_ext_address
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(ext_address))
    {
        return node_id.clone();
    }

    let id = format!("unknown_{}", ext_address);
    if !graph_nodes.iter().any(|existing| existing.id == id) {
        graph_nodes.push(NetworkTopologyNode {
            id: id.clone(),
            kind: "thread_unknown".into(),
            network_type: "thread".into(),
            node_id: None,
            role: None,
            available: None,
            is_bridge: None,
            ext_address: Some(ext_address.to_string()),
            rloc16,
            // An unknown neighbour is on the same mesh as whoever saw it.
            ext_pan_id: thread_attribute(observer, thread_attr::EXTENDED_PAN_ID)
                .and_then(Value::as_u64)
                .map(|value| format!("{:016X}", value)),
            network_name: thread_attribute(observer, thread_attr::NETWORK_NAME)
                .and_then(Value::as_str)
                .map(str::to_string),
            ssid: None,
            bssid: None,
            host_name: None,
            vendor_name: None,
            model_name: None,
            last_seen: None,
        });
    }
    id
}

/// Thread links are directional and often reported from both ends; fold the
/// second sighting into the first rather than drawing two edges.
fn merge_connection(
    connections: &mut Vec<NetworkTopologyConnection>,
    connection: NetworkTopologyConnection,
) {
    if let Some(existing) = connections
        .iter_mut()
        .find(|existing| links_same_pair(existing, &connection.source, &connection.target))
    {
        if existing.source == connection.source {
            existing.source_to_target = connection.source_to_target;
        } else {
            existing.target_to_source = connection.source_to_target;
        }
        existing.strength = strongest(&existing.strength, &connection.strength).to_string();
        return;
    }
    connections.push(connection);
}

fn links_same_pair(connection: &NetworkTopologyConnection, a: &str, b: &str) -> bool {
    (connection.source == a && connection.target == b)
        || (connection.source == b && connection.target == a)
}

fn strength_rank(strength: &str) -> u8 {
    match strength {
        "strong" => 4,
        "medium" => 3,
        "weak" => 2,
        "none" => 1,
        _ => 0,
    }
}

fn strongest<'a>(a: &'a str, b: &'a str) -> &'a str {
    if strength_rank(a) >= strength_rank(b) {
        a
    } else {
        b
    }
}

/// Thread Link Quality Indicator, 0-3 on OpenThread.
fn thread_strength(lqi: u8) -> &'static str {
    match lqi {
        0 => "none",
        1 => "weak",
        2 => "medium",
        _ => "strong",
    }
}

fn wifi_strength(rssi: i16) -> &'static str {
    match rssi {
        rssi if rssi >= -55 => "strong",
        rssi if rssi >= -67 => "medium",
        rssi if rssi >= -80 => "weak",
        _ => "none",
    }
}

/// ThreadNetworkDiagnostics `RoutingRole`.
fn routing_role(role: u64) -> String {
    match role {
        1 => "unassigned",
        2 => "sleepy_end_device",
        3 => "end_device",
        4 => "reed",
        5 => "router",
        6 => "leader",
        _ => "unassigned",
    }
    .to_string()
}

fn thread_attribute(node: &MatterNodeData, attribute: u32) -> Option<&Value> {
    node.attributes
        .get(&format!("0/{}/{}", THREAD_DIAGNOSTICS_CLUSTER, attribute))
}

fn wifi_attribute(node: &MatterNodeData, attribute: u32) -> Option<&Value> {
    node.attributes
        .get(&format!("0/{}/{}", WIFI_DIAGNOSTICS_CLUSTER, attribute))
}

/// Extended addresses are octet strings, which arrive base64-encoded.
fn thread_string(node: &MatterNodeData, attribute: u32) -> Option<String> {
    thread_attribute(node, attribute)
        .and_then(Value::as_str)
        .and_then(decode_ext_address)
}

fn decode_ext_address(encoded: &str) -> Option<String> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    let bytes = BASE64.decode(encoded).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(bytes.iter().map(|byte| format!("{:02X}", byte)).collect())
}

/// BSSIDs are 6-byte octet strings rendered as `AA:BB:CC:DD:EE:FF`.
fn format_bssid(value: &Value) -> Option<String> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    let bytes = BASE64.decode(value.as_str()?).ok()?;
    if bytes.len() != 6 || bytes.iter().all(|byte| *byte == 0) {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|byte| format!("{:02X}", byte))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::{call, test_context};
    use futures_lite::future::block_on;

    fn base64(bytes: &[u8]) -> String {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine as _;
        BASE64.encode(bytes)
    }

    fn thread_node(node_id: u64, ext: &[u8; 8], role: u64) -> MatterNodeData {
        let mut node = MatterNodeData::new(node_id, "2026-01-01T00:00:00.000Z".into());
        node.attributes.insert("0/53/1".into(), json!(role));
        node.attributes
            .insert("0/53/2".into(), json!("MyThreadNet"));
        node.attributes
            .insert("0/53/4".into(), json!(0x1122334455667788u64));
        node.attributes.insert("0/53/63".into(), json!(base64(ext)));
        node
    }

    /// `collected_at` moves on every rebuild, so a graph compared with it
    /// would look different every time and publish on every poll.
    #[test]
    fn the_compared_shape_ignores_when_it_was_collected() {
        let nodes = [thread_node(1, &[0xAA; 8], 6)];
        let mut early = build_topology(&nodes);
        let mut late = build_topology(&nodes);
        early.collected_at = 1;
        late.collected_at = 2;
        assert_eq!(topology_shape(&early), topology_shape(&late));

        let grown = build_topology(&[nodes[0].clone(), thread_node(2, &[0xBB; 8], 5)]);
        assert_ne!(topology_shape(&early), topology_shape(&grown));
    }

    /// The watcher publishes on a node change, and only when the graph moved.
    #[test]
    fn a_node_change_publishes_the_graph_once() {
        let context = Arc::new(test_context());
        context.nodes.upsert(crate::storage::StoredNode::new(
            thread_node(1, &[0xAA; 8], 6),
        ));
        // Subscribed before the watcher runs, so nothing is missed either way.
        let watched = context.events.subscribe();
        let seen = context.events.subscribe();

        let driver = async {
            let node = context.nodes.get(1).unwrap();
            context.events.publish(Event::node_updated(&node));

            let mut topologies = 0;
            let mut node_events = 0;
            // Two rounds: the second publishes a node event that changes
            // nothing, so the graph must not be announced again.
            while node_events < 2 {
                let event = seen.recv().await.unwrap();
                match event.name() {
                    "network_topology_updated" => {
                        topologies += 1;
                        assert_eq!(event.payload["data"]["nodes"].as_array().unwrap().len(), 1);
                        // Nothing about the store changed, so this must be
                        // the last one.
                        context.events.publish(Event::node_updated(&node));
                    }
                    "node_updated" => node_events += 1,
                    other => panic!("unexpected event {}", other),
                }
                // Let the watcher run before deciding it published nothing.
                for _ in 0..8 {
                    futures_lite::future::yield_now().await;
                }
            }
            topologies
        };

        let watcher = async {
            watch(context.clone(), watched).await;
            0
        };
        assert_eq!(block_on(futures_lite::future::or(driver, watcher)), 1);
    }

    #[test]
    fn a_thread_mesh_becomes_nodes_and_edges() {
        let leader_ext = [0xAA; 8];
        let router_ext = [0xBB; 8];
        let mut leader = thread_node(1, &leader_ext, 6);
        leader.attributes.insert(
            "0/53/7".into(),
            json!([{ "0": base64(&router_ext), "2": 4096, "5": 3, "6": -45 }]),
        );
        let router = thread_node(2, &router_ext, 5);

        let topology = build_topology(&[leader, router]);
        assert_eq!(topology.nodes.len(), 2);
        assert_eq!(topology.nodes[0].role.as_deref(), Some("leader"));
        assert_eq!(topology.nodes[1].role.as_deref(), Some("router"));
        assert_eq!(
            topology.nodes[0].ext_address.as_deref(),
            Some("AAAAAAAAAAAAAAAA")
        );
        assert_eq!(
            topology.nodes[0].ext_pan_id.as_deref(),
            Some("1122334455667788")
        );

        assert_eq!(topology.connections.len(), 1);
        let edge = &topology.connections[0];
        assert_eq!(edge.source, "1");
        assert_eq!(edge.target, "2");
        assert_eq!(edge.strength, "strong");
        assert_eq!(edge.source_to_target.as_ref().unwrap().lqi, Some(3));
        assert_eq!(edge.source_to_target.as_ref().unwrap().rssi, Some(-45));
    }

    #[test]
    fn a_neighbour_off_this_fabric_becomes_an_unknown_node() {
        let mut node = thread_node(1, &[0xAA; 8], 6);
        node.attributes.insert(
            "0/53/7".into(),
            json!([{ "0": base64(&[0xCC; 8]), "5": 1 }]),
        );
        let topology = build_topology(&[node]);
        assert_eq!(topology.nodes.len(), 2);
        let unknown = &topology.nodes[1];
        assert_eq!(unknown.kind, "thread_unknown");
        assert_eq!(unknown.id, "unknown_CCCCCCCCCCCCCCCC");
        assert_eq!(topology.connections[0].strength, "weak");
    }

    #[test]
    fn links_seen_from_both_ends_become_one_edge() {
        let a_ext = [0xAA; 8];
        let b_ext = [0xBB; 8];
        let mut a = thread_node(1, &a_ext, 6);
        a.attributes
            .insert("0/53/7".into(), json!([{ "0": base64(&b_ext), "5": 2 }]));
        let mut b = thread_node(2, &b_ext, 5);
        b.attributes
            .insert("0/53/7".into(), json!([{ "0": base64(&a_ext), "5": 3 }]));

        let topology = build_topology(&[a, b]);
        assert_eq!(topology.connections.len(), 1);
        let edge = &topology.connections[0];
        // The summary strength is the strongest direction seen.
        assert_eq!(edge.strength, "strong");
        assert!(edge.source_to_target.is_some());
        assert!(edge.target_to_source.is_some());
    }

    #[test]
    fn wifi_nodes_hang_off_a_synthetic_access_point() {
        let mut node = MatterNodeData::new(7, "2026-01-01T00:00:00.000Z".into());
        node.attributes.insert(
            "0/54/0".into(),
            json!(base64(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66])),
        );
        node.attributes.insert("0/54/4".into(), json!(-52));

        let topology = build_topology(&[node]);
        assert_eq!(topology.nodes.len(), 2);
        assert_eq!(topology.nodes[0].network_type, "wifi");
        assert_eq!(topology.nodes[0].role.as_deref(), Some("station"));
        assert_eq!(topology.nodes[1].id, "ap_112233445566");
        assert_eq!(topology.nodes[1].kind, "wifi_ap");
        assert_eq!(
            topology.nodes[1].bssid.as_deref(),
            Some("11:22:33:44:55:66")
        );
        assert_eq!(topology.connections[0].strength, "strong");
    }

    #[test]
    fn a_node_with_no_diagnostics_is_still_in_the_graph() {
        let node = MatterNodeData::new(3, "2026-01-01T00:00:00.000Z".into());
        let topology = build_topology(&[node]);
        assert_eq!(topology.nodes.len(), 1);
        assert_eq!(topology.nodes[0].network_type, "unknown");
        assert!(topology.connections.is_empty());
    }

    #[test]
    fn meshcop_answers_map_onto_the_border_router_shape() {
        let mut instance = ServiceInstance {
            instance_name: "OpenThread BorderRouter".into(),
            host_name: Some("Cuisine.local".into()),
            port: Some(49191),
            addresses: vec!["fd00::1".parse().unwrap()],
            ..ServiceInstance::default()
        };
        instance.txt.insert("nn".into(), b"MyThreadNet".to_vec());
        instance.txt.insert(
            "xp".into(),
            vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        );
        instance.txt.insert("xa".into(), vec![0xAA; 8]);
        instance.txt.insert("vn".into(), b"OpenThread".to_vec());

        let entry = border_router(&instance, 1_700_000_000_000);
        let wire = serde_json::to_value(&entry).unwrap();
        assert_eq!(wire["extendedPanIdHex"], json!("1122334455667788"));
        assert_eq!(wire["extAddressHex"], json!("AAAAAAAAAAAAAAAA"));
        assert_eq!(wire["networkName"], json!("MyThreadNet"));
        // The host name is a display label, without the .local suffix.
        assert_eq!(wire["hostname"], json!("Cuisine"));
        assert_eq!(wire["addresses"], json!(["fd00::1"]));
        assert_eq!(wire["sources"], json!(["mdns"]));
        assert_eq!(wire["lastSeen"], json!(1_700_000_000_000i64));
        assert_eq!(wire["vendorName"], json!("OpenThread"));
        // Absent fields are omitted rather than nulled.
        assert!(wire.get("modelName").is_none());
    }

    #[test]
    fn thread_diagnostics_validate_the_ext_pan_id() {
        let context = test_context();
        let args = Args::new(json!({ "ext_pan_id": "xyz" }));
        let error = block_on(get_thread_diagnostics(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(error.details.contains("16 hex characters"));

        let args = Args::new(json!({ "ext_pan_id": "1122334455667788" }));
        assert_eq!(
            block_on(get_thread_diagnostics(&args, call(&context))).unwrap(),
            Value::Null
        );
        assert_eq!(
            block_on(get_thread_diagnostics(&Args::default(), call(&context))).unwrap(),
            json!([])
        );
    }

    #[test]
    fn wifi_and_thread_strengths_follow_the_documented_buckets() {
        assert_eq!(thread_strength(0), "none");
        assert_eq!(thread_strength(1), "weak");
        assert_eq!(thread_strength(2), "medium");
        assert_eq!(thread_strength(3), "strong");
        assert_eq!(wifi_strength(-40), "strong");
        assert_eq!(wifi_strength(-65), "medium");
        assert_eq!(wifi_strength(-75), "weak");
        assert_eq!(wifi_strength(-95), "none");
    }
}
