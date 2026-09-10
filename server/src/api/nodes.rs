//! Node listing, lifecycle, and per-node maintenance commands.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::events::Event;
use crate::protocol::message::Args;
use crate::protocol::model::{AttributesData, MatterNodeData, NodePingResult, TEST_NODE_START};
use crate::storage::StoredNode;

use crate::matter::mdns_browser;

use super::{now_iso, require_node, CallContext, ServerContext};

/// Vendor names, keyed by decimal vendor id exactly as the reference reports
/// them. Lifted from the reference's own static table.
const VENDOR_JSON: &str = include_str!("vendors.json");

/// How long to wait for a node's own answer to an operational mDNS query. It
/// is answered by the device (or, for a Thread node, by its border router's
/// advertisement), so it is a local round-trip and not worth waiting long for.
const OPERATIONAL_RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// OperationalCredentials cluster: the fabric index the *node* assigned to us.
const OPERATIONAL_CREDENTIALS_CLUSTER: u32 = 62;
const CURRENT_FABRIC_INDEX_ATTRIBUTE: u32 = 5;
const REMOVE_FABRIC_COMMAND: u32 = 10;

pub async fn start_listening(_args: &Args, context: CallContext<'_>) -> ApiResult {
    // The connection is marked as listening by the connection loop; the
    // response is the current node list, which is what the client uses to
    // build its initial state.
    Ok(serde_json::to_value(context.server.nodes.all()).unwrap_or(Value::Null))
}

pub async fn get_nodes(args: &Args, context: CallContext<'_>) -> ApiResult {
    let only_available = args.bool_or("only_available", false)?;
    Ok(
        serde_json::to_value(context.server.nodes.all_filtered(only_available))
            .unwrap_or(Value::Null),
    )
}

pub async fn get_node(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = args.req_u64("node_id")?;
    let node = context
        .server
        .nodes
        .get(node_id)
        .ok_or_else(|| ApiError::node_not_exists(node_id))?;
    Ok(serde_json::to_value(node).unwrap_or(Value::Null))
}

/// Report the addresses the node can be reached on.
///
/// `prefer_cache` answers from what was last recorded. Without it the node's
/// operational instance is resolved over mDNS, which is both the fresher
/// answer and the only one a Bluetooth-commissioned node ever had: rs-matter
/// resolves that address internally during commissioning and does not expose
/// it, so nothing else ever learns it.
///
/// A resolve that comes back empty falls back to the recorded addresses and a
/// reachability check rather than reporting none: a device that is answering
/// CASE but did not answer this one query is reachable, whatever mDNS says.
pub async fn get_node_ip_addresses(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let prefer_cache = args.bool_or("prefer_cache", false)?;

    if prefer_cache {
        return Ok(json!(context.server.nodes.ip_addresses(node_id)));
    }

    if let Some(resolved) = resolve_addresses(context.server, node_id).await {
        return Ok(json!(resolved));
    }

    let addresses = context.server.nodes.ip_addresses(node_id);
    if !addresses.is_empty() && context.server.matter.ping(node_id).await.is_err() {
        return Ok(json!([]));
    }
    Ok(json!(addresses))
}

/// Resolve a node's operational addresses over mDNS and record them.
///
/// `None` means nothing answered — a sleepy device, a node that is off, or an
/// answer that did not reach this host — which is not the same as a node with
/// no addresses, so the caller decides what to report.
pub async fn resolve_addresses(context: &ServerContext, node_id: u64) -> Option<Vec<String>> {
    // An imported test node has no device behind it, so there is nothing on
    // the network to answer for it.
    if node_id >= TEST_NODE_START {
        return None;
    }
    let fabric = context.fabric_info().await.ok()?;
    let addresses = mdns_browser::resolve_operational(
        fabric.compressed_fabric_id,
        node_id,
        OPERATIONAL_RESOLVE_TIMEOUT,
    )
    .await
    .unwrap_or_default();
    if addresses.is_empty() {
        return None;
    }

    let addresses: Vec<String> = addresses
        .into_iter()
        .map(|address| address.to_string())
        .collect();
    if context.nodes.ip_addresses(node_id) != addresses {
        context.nodes.set_ip_addresses(node_id, addresses.clone());
        if let Err(error) = context.nodes.save() {
            log::warn!("Could not persist node {}'s addresses: {}", node_id, error);
        }
    }
    Some(addresses)
}

pub async fn ping_node(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let attempts = args.u8("attempts")?.unwrap_or(1).clamp(1, 10);

    let mut reachable = false;
    for _ in 0..attempts {
        if context.server.matter.ping(node_id).await.is_ok() {
            reachable = true;
            break;
        }
    }

    // Availability follows a ping: it is the most direct evidence there is.
    if let Some((node, changed)) = context.server.nodes.set_available(node_id, reachable) {
        if changed {
            let _ = context.server.nodes.save();
            context.server.events.publish(Event::node_updated(&node));
        }
    }

    let addresses = context.server.nodes.ip_addresses(node_id);
    let mut result = NodePingResult::new();
    if addresses.is_empty() {
        // No address was ever recorded (an imported node, or one commissioned
        // by an older build). Report the reachability under the node's
        // operational instance name so the map is never misleadingly empty.
        result.insert(format!("node-{}", node_id), reachable);
    } else {
        for address in addresses {
            result.insert(address, reachable);
        }
    }
    Ok(serde_json::to_value(result).unwrap_or(Value::Null))
}

/// Decommission a node: remove this controller's fabric from the device, then
/// forget it locally.
///
/// The device round-trip is best effort. A node that is already unplugged can
/// still be removed from the controller — refusing would leave the user with an
/// entry they cannot delete — but it keeps our fabric until it is factory
/// reset.
pub async fn remove_node(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = args.req_u64("node_id")?;
    let stored = context
        .server
        .nodes
        .get_stored(node_id)
        .ok_or_else(|| ApiError::node_not_exists(node_id))?;

    if !stored.data.is_test_node() {
        match remove_our_fabric(&stored, node_id, context).await {
            Ok(()) => log::info!("Removed this controller's fabric from node {}", node_id),
            Err(error) => log::warn!(
                "Node {} could not be decommissioned cleanly ({}); removing it locally anyway",
                node_id,
                error
            ),
        }
    }

    context.server.nodes.remove(node_id);
    context
        .server
        .nodes
        .save()
        .map_err(|e| ApiError::sdk(format!("Failed to persist the node removal: {}", e)))?;
    context.server.events.publish(Event::node_removed(node_id));
    Ok(Value::Null)
}

async fn remove_our_fabric(
    stored: &StoredNode,
    node_id: u64,
    context: CallContext<'_>,
) -> Result<(), ApiError> {
    // Prefer what the device reports right now; fall back to what was recorded
    // at commissioning for a device that answers the invoke but not the read.
    let fabric_index = match context
        .server
        .matter
        .read_attributes(
            node_id,
            vec![rs_matter::im::AttrPath::from_gp(
                &rs_matter::im::GenericPath::new(
                    Some(0),
                    Some(OPERATIONAL_CREDENTIALS_CLUSTER),
                    Some(CURRENT_FABRIC_INDEX_ATTRIBUTE),
                ),
            )],
            false,
        )
        .await
    {
        Ok(attributes) => attributes
            .get("0/62/5")
            .and_then(Value::as_u64)
            .map(|index| index as u8)
            .or(stored.device_fabric_index),
        Err(_) => stored.device_fabric_index,
    };

    let fabric_index = fabric_index.ok_or_else(|| {
        ApiError::sdk("The fabric index this controller holds on the node is unknown")
    })?;

    context
        .server
        .matter
        .invoke(
            node_id,
            0,
            OPERATIONAL_CREDENTIALS_CLUSTER,
            REMOVE_FABRIC_COMMAND,
            crate::matter::tlv_json::TlvNode::Struct(vec![(
                0,
                crate::matter::tlv_json::TlvNode::U64(fabric_index as u64),
            )]),
            None,
            BTreeMap::new(),
        )
        .await
        .map(|_| ())
}

pub async fn interview_node(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    if context
        .server
        .nodes
        .get(node_id)
        .map(|node| node.is_test_node())
        .unwrap_or(false)
    {
        // A test node has no device behind it; re-interviewing is a no-op
        // rather than an error.
        return Ok(Value::Null);
    }
    let attributes = context.server.matter.interview(node_id).await?;
    apply_interview(context.server, node_id, attributes)?;
    Ok(Value::Null)
}

/// Merge interview results into the store and publish everything that changed.
///
/// The event fan-out is what makes a client's cached view converge without a
/// full refetch, so each individual attribute change is reported, not just the
/// node as a whole.
pub fn apply_interview(
    context: &ServerContext,
    node_id: u64,
    attributes: AttributesData,
) -> Result<MatterNodeData, ApiError> {
    let diff = context
        .nodes
        .apply_interview(node_id, attributes, now_iso())
        .ok_or_else(|| ApiError::node_not_exists(node_id))?;
    context
        .nodes
        .save()
        .map_err(|e| ApiError::sdk(format!("Failed to persist the interview: {}", e)))?;

    for endpoint in &diff.endpoints_added {
        context
            .events
            .publish(Event::endpoint_added(node_id, *endpoint));
    }
    for endpoint in &diff.endpoints_removed {
        context
            .events
            .publish(Event::endpoint_removed(node_id, *endpoint));
    }
    for (path, value) in &diff.changed_attributes {
        context
            .events
            .publish(Event::attribute_updated(node_id, path, value.clone()));
    }
    context.events.publish(Event::node_updated(&diff.node));
    Ok(diff.node)
}

/// Import nodes from a Home Assistant diagnostics dump.
///
/// Imported nodes are assigned ids in the reserved test range so they can never
/// collide with a real commissioned node.
pub async fn import_test_node(args: &Args, context: CallContext<'_>) -> ApiResult {
    let dump = args.req_str("dump")?;
    let parsed: Value = serde_json::from_str(dump)
        .map_err(|e| ApiError::invalid_args(format!("Invalid dump: {}", e)))?;

    let dumped_nodes = extract_dump_nodes(&parsed)
        .ok_or_else(|| ApiError::invalid_args("Invalid dump format: cannot find node data"))?;

    let first_id = context
        .server
        .nodes
        .all()
        .iter()
        .filter(|node| node.is_test_node())
        .map(|node| node.node_id + 1)
        .max()
        .unwrap_or(TEST_NODE_START);

    for (offset, dumped) in dumped_nodes.into_iter().enumerate() {
        let node_id = first_id + offset as u64;

        let mut node = MatterNodeData::new(node_id, now_iso());
        if let Some(date) = dumped.get("date_commissioned").and_then(Value::as_str) {
            node.date_commissioned = date.to_string();
        }
        if let Some(date) = dumped.get("last_interview").and_then(Value::as_str) {
            node.last_interview = date.to_string();
        }
        node.interview_version = dumped
            .get("interview_version")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        node.available = dumped
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        node.is_bridge = dumped
            .get("is_bridge")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(attributes) = dumped.get("attributes").and_then(Value::as_object) {
            node.attributes = attributes
                .iter()
                .map(|(path, value)| (path.clone(), value.clone()))
                .collect();
        }

        context.server.nodes.upsert(StoredNode::new(node.clone()));
        context.server.events.publish(Event::node_added(&node));
        log::info!(
            "Imported test node {} with {} attributes",
            node_id,
            node.attributes.len()
        );
    }

    context
        .server
        .nodes
        .save()
        .map_err(|e| ApiError::sdk(format!("Failed to persist the imported nodes: {}", e)))?;
    Ok(Value::Null)
}

/// Home Assistant writes dumps in three shapes; all three are accepted.
fn extract_dump_nodes(dump: &Value) -> Option<Vec<&Value>> {
    let data = dump.get("data")?;
    if let Some(node) = data.get("node") {
        return Some(vec![node]);
    }
    let nodes = data
        .get("server")
        .and_then(|server| server.get("nodes"))
        .or_else(|| data.get("nodes"))?;
    match nodes {
        Value::Array(items) => Some(items.iter().collect()),
        Value::Object(map) => Some(map.values().collect()),
        _ => None,
    }
}

/// Vendor names by decimal vendor id.
///
/// The reference merges this static table with a live DCL lookup; without a
/// vendor-registry client the static table is what this server has, so a very
/// new vendor may be missing rather than wrong.
pub async fn get_vendor_names(args: &Args, context: CallContext<'_>) -> ApiResult {
    let all = vendors();
    let Some(filter) = args.u64_array("filter_vendors")? else {
        return Ok(serde_json::to_value(all).unwrap_or(Value::Null));
    };
    if filter.is_empty() {
        return Ok(serde_json::to_value(all).unwrap_or(Value::Null));
    }
    let mut result = BTreeMap::new();
    for vendor_id in filter {
        let key = vendor_id.to_string();
        if let Some(name) = all.get(&key) {
            result.insert(key, name.clone());
            continue;
        }
        // Not in the table the reference ships. A vendor id is assigned in the
        // ledger, so a vendor that shipped after that table was cut is still
        // nameable — and an id nobody holds is simply absent from the answer,
        // exactly as it is today.
        if let Ok(vendor_id) = u16::try_from(vendor_id) {
            if let Some(name) = context.server.vendor_name_from_dcl(vendor_id) {
                result.insert(key, name);
            }
        }
    }
    Ok(serde_json::to_value(result).unwrap_or(Value::Null))
}

/// Look up a single vendor name, used when decorating fabric descriptors.
pub fn vendor_name(vendor_id: u16) -> Option<String> {
    vendors().get(&vendor_id.to_string()).cloned()
}

fn vendors() -> &'static BTreeMap<String, String> {
    static VENDORS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    VENDORS.get_or_init(|| {
        serde_json::from_str(VENDOR_JSON).expect("bundled vendor table is valid JSON")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::{call, test_context};
    use futures_lite::future::block_on;

    fn seed_node(context: &ServerContext, node_id: u64) {
        let mut stored = StoredNode::new(MatterNodeData::new(
            node_id,
            "2026-01-01T00:00:00.000Z".into(),
        ));
        stored.ip_addresses = vec!["fd00::1".into()];
        context.nodes.upsert(stored);
    }

    #[test]
    fn get_node_reports_a_missing_node_with_the_documented_code() {
        let context = test_context();
        let args = Args::new(json!({ "node_id": 9 }));
        let error = block_on(get_node(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 5);
        assert_eq!(error.details, "Node 9 does not exist");
    }

    #[test]
    fn get_nodes_filters_on_availability() {
        let context = test_context();
        seed_node(&context, 1);
        seed_node(&context, 2);
        context.nodes.set_available(2, false);

        let all = block_on(get_nodes(&Args::default(), call(&context))).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 2);

        let args = Args::new(json!({ "only_available": true }));
        let available = block_on(get_nodes(&args, call(&context))).unwrap();
        assert_eq!(available.as_array().unwrap().len(), 1);
        assert_eq!(available[0]["node_id"], json!(1));
    }

    #[test]
    fn start_listening_returns_the_node_list() {
        let context = test_context();
        seed_node(&context, 4);
        let result = block_on(start_listening(&Args::default(), call(&context))).unwrap();
        assert_eq!(result[0]["node_id"], json!(4));
        // The wire model must not leak the controller's internal fields.
        assert!(result[0].get("ip_addresses").is_none());
    }

    /// A test node has no device, so the lookup must not reach the network —
    /// which is also what keeps this test from depending on one.
    #[test]
    fn a_test_node_is_never_resolved_over_mdns() {
        let context = test_context();
        let node_id = TEST_NODE_START + 1;
        context
            .nodes
            .upsert(StoredNode::new(MatterNodeData::new(node_id, now_iso())));
        assert_eq!(block_on(resolve_addresses(&context, node_id)), None);
    }

    #[test]
    fn cached_ip_addresses_are_returned_verbatim() {
        let context = test_context();
        seed_node(&context, 1);
        let args = Args::new(json!({ "node_id": 1, "prefer_cache": true }));
        let result = block_on(get_node_ip_addresses(&args, call(&context))).unwrap();
        assert_eq!(result, json!(["fd00::1"]));
    }

    #[test]
    fn removing_an_offline_node_still_forgets_it_locally() {
        let context = test_context();
        seed_node(&context, 1);
        let receiver = context.events.subscribe();
        let args = Args::new(json!({ "node_id": 1 }));

        // The Matter actor is not running, so the decommission round-trip
        // fails; the node must still disappear.
        assert_eq!(
            block_on(remove_node(&args, call(&context))).unwrap(),
            Value::Null
        );
        assert!(!context.nodes.contains(1));
        let event = block_on(receiver.recv()).unwrap();
        assert_eq!(event.name(), "node_removed");
        assert_eq!(event.payload["data"], json!(1));
    }

    #[test]
    fn an_interview_publishes_the_changes_it_found() {
        let context = test_context();
        seed_node(&context, 1);
        let receiver = context.events.subscribe();

        let mut attributes = AttributesData::new();
        attributes.insert("0/40/1".into(), json!("ACME"));
        attributes.insert("1/6/0".into(), json!(true));
        apply_interview(&context, 1, attributes).unwrap();

        let mut seen = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            seen.push(event.name().to_string());
        }
        assert_eq!(
            seen,
            vec![
                "endpoint_added",
                "endpoint_added",
                "attribute_updated",
                "attribute_updated",
                "node_updated"
            ]
        );
    }

    #[test]
    fn imported_nodes_land_in_the_reserved_id_range() {
        let context = test_context();
        let dump = json!({
            "data": {
                "node": {
                    "node_id": 4,
                    "date_commissioned": "2026-01-01T00:00:00.000Z",
                    "last_interview": "2026-01-02T00:00:00.000Z",
                    "interview_version": 6,
                    "available": true,
                    "is_bridge": false,
                    "attributes": { "0/40/1": "ACME" }
                }
            }
        });
        let args = Args::new(json!({ "dump": dump.to_string() }));
        assert_eq!(
            block_on(import_test_node(&args, call(&context))).unwrap(),
            Value::Null
        );

        let nodes = context.nodes.all();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, TEST_NODE_START);
        assert!(nodes[0].is_test_node());
        assert_eq!(nodes[0].attributes["0/40/1"], json!("ACME"));
        // The original id is replaced, not preserved.
        assert!(!context.nodes.contains(4));

        // A second import takes the next id in the range.
        block_on(import_test_node(&args, call(&context))).unwrap();
        assert!(context.nodes.contains(TEST_NODE_START + 1));
    }

    #[test]
    fn a_dump_without_node_data_is_an_argument_error() {
        let context = test_context();
        let args = Args::new(json!({ "dump": "{\"data\":{}}" }));
        let error = block_on(import_test_node(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(error.details.contains("cannot find node data"));
    }

    #[test]
    fn vendor_names_are_keyed_by_decimal_id_and_filterable() {
        let context = test_context();
        let args = Args::new(json!({ "filter_vendors": [4874, 65521] }));
        let result = block_on(get_vendor_names(&args, call(&context))).unwrap();
        assert_eq!(result["4874"], json!("EVE SYSTEMS"));
        assert_eq!(result.as_object().unwrap().len(), 2);

        let all = block_on(get_vendor_names(&Args::default(), call(&context))).unwrap();
        assert!(all.as_object().unwrap().len() > 1000);
        assert_eq!(all["0"], json!("[Matter Standard]"));
    }

    /// Hits the real CSA ledger; run with `--ignored` when online. 161 vendor
    /// ids the ledger has assigned are missing from the reference's table,
    /// and this is one of them.
    #[test]
    #[ignore]
    fn a_vendor_missing_from_the_static_table_is_named_from_the_ledger() {
        let context = test_context();
        assert!(!vendors().contains_key("5687"));
        let args = Args::new(json!({ "filter_vendors": [5687] }));
        let result = block_on(get_vendor_names(&args, call(&context))).unwrap();
        assert_eq!(result["5687"], json!("NVIDIA"));
    }
}
