//! The commissioned-node store.
//!
//! Nodes are held in the exact shape they go on the wire, plus a small sidecar
//! of controller-only data (last known addresses) that must never leak into a
//! `MatterNodeData` response. Keeping the split explicit is what stops the
//! internal fields from drifting onto the protocol.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::model::{AttributesData, MatterNodeData};

/// A node plus controller-only bookkeeping.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredNode {
    pub data: MatterNodeData,
    /// Last known operational addresses, used by `get_node_ip_addresses` with
    /// `prefer_cache` and as a commissioning fallback.
    #[serde(default)]
    pub ip_addresses: Vec<String>,
    /// The fabric slot *the device* assigned to this controller, recorded at
    /// commissioning. Removing ourselves from the node needs it, and an
    /// offline node cannot be asked for it.
    #[serde(default)]
    pub device_fabric_index: Option<u8>,
}

impl StoredNode {
    pub fn new(data: MatterNodeData) -> Self {
        Self {
            data,
            ip_addresses: Vec::new(),
            device_fabric_index: None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Snapshot {
    #[serde(default)]
    nodes: Vec<StoredNode>,
}

/// Thread-safe, optionally file-backed node store.
#[derive(Default)]
pub struct NodeStore {
    nodes: std::sync::Mutex<BTreeMap<u64, StoredNode>>,
    path: Option<PathBuf>,
}

impl NodeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a snapshot. A missing file is an empty store; a malformed one is
    /// reported rather than silently discarded, because losing the node list
    /// means every device has to be re-commissioned.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let nodes = if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading node state {}", path.display()))?;
            let snapshot: Snapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing node state {}", path.display()))?;
            snapshot
                .nodes
                .into_iter()
                .map(|node| (node.data.node_id, node))
                .collect()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            nodes: std::sync::Mutex::new(nodes),
            path: Some(path),
        })
    }

    /// Write the snapshot through a temporary file so an interrupted write
    /// cannot truncate the existing state.
    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let snapshot = Snapshot {
            nodes: self.nodes.lock().unwrap().values().cloned().collect(),
        };
        super::private::write_private(path, &serde_json::to_vec_pretty(&snapshot)?)
            .with_context(|| format!("writing node state {}", path.display()))?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, node_id: u64) -> bool {
        self.nodes.lock().unwrap().contains_key(&node_id)
    }

    pub fn get(&self, node_id: u64) -> Option<MatterNodeData> {
        self.nodes
            .lock()
            .unwrap()
            .get(&node_id)
            .map(|node| node.data.clone())
    }

    pub fn get_stored(&self, node_id: u64) -> Option<StoredNode> {
        self.nodes.lock().unwrap().get(&node_id).cloned()
    }

    /// All nodes, ordered by node id so responses are stable across calls.
    pub fn all(&self) -> Vec<MatterNodeData> {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .map(|node| node.data.clone())
            .collect()
    }

    pub fn all_filtered(&self, only_available: bool) -> Vec<MatterNodeData> {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .filter(|node| !only_available || node.data.available)
            .map(|node| node.data.clone())
            .collect()
    }

    pub fn highest_node_id(&self) -> Option<u64> {
        self.nodes
            .lock()
            .unwrap()
            .keys()
            .rev()
            .find(|id| **id < crate::protocol::model::TEST_NODE_START)
            .copied()
    }

    /// Whether this node has never been read from.
    ///
    /// True for a node adopted from another server: it is known to be on the
    /// fabric, but nothing has been read off the device yet. The first
    /// successful poll of such a node is its interview, not an update.
    pub fn awaiting_first_interview(&self, node_id: u64) -> bool {
        self.nodes
            .lock()
            .unwrap()
            .get(&node_id)
            .map(|node| node.data.attributes.is_empty() && node.data.interview_version == 0)
            .unwrap_or(false)
    }

    pub fn upsert(&self, node: StoredNode) {
        self.nodes.lock().unwrap().insert(node.data.node_id, node);
    }

    pub fn remove(&self, node_id: u64) -> bool {
        self.nodes.lock().unwrap().remove(&node_id).is_some()
    }

    /// Apply a mutation to one node, returning the updated wire model so the
    /// caller can publish a `node_updated` event without a second lookup.
    pub fn update(&self, node_id: u64, f: impl FnOnce(&mut StoredNode)) -> Option<MatterNodeData> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&node_id)?;
        f(node);
        Some(node.data.clone())
    }

    /// Record availability, reporting whether it actually changed so callers
    /// only emit an event on a real transition.
    pub fn set_available(&self, node_id: u64, available: bool) -> Option<(MatterNodeData, bool)> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&node_id)?;
        let changed = node.data.available != available;
        node.data.available = available;
        Some((node.data.clone(), changed))
    }

    pub fn set_ip_addresses(&self, node_id: u64, addresses: Vec<String>) {
        if let Some(node) = self.nodes.lock().unwrap().get_mut(&node_id) {
            node.ip_addresses = addresses;
        }
    }

    pub fn ip_addresses(&self, node_id: u64) -> Vec<String> {
        self.nodes
            .lock()
            .unwrap()
            .get(&node_id)
            .map(|node| node.ip_addresses.clone())
            .unwrap_or_default()
    }

    /// Merge interview results, reporting which individual attributes changed
    /// and which endpoints appeared or disappeared. The caller turns those into
    /// `attribute_updated` / `endpoint_added` / `endpoint_removed` events.
    pub fn apply_interview(
        &self,
        node_id: u64,
        attributes: AttributesData,
        interviewed_at: String,
    ) -> Option<InterviewDiff> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&node_id)?;

        let endpoints_before = node.data.endpoints();
        let mut changed = Vec::new();
        for (path, value) in &attributes {
            if node.data.attributes.get(path) != Some(value) {
                changed.push((path.clone(), value.clone()));
            }
        }
        let removed_paths: Vec<String> = node
            .data
            .attributes
            .keys()
            .filter(|path| !attributes.contains_key(*path))
            .cloned()
            .collect();

        node.data.attributes = attributes;
        node.data.last_interview = interviewed_at;
        node.data.interview_version += 1;
        node.data.available = true;
        node.data.is_bridge = detect_bridge(&node.data.attributes);
        node.data.matter_version = detect_matter_version(&node.data.attributes);

        let endpoints_after = node.data.endpoints();
        let endpoints_added: Vec<u16> = endpoints_after
            .iter()
            .filter(|endpoint| !endpoints_before.contains(endpoint))
            .copied()
            .collect();
        let endpoints_removed: Vec<u16> = endpoints_before
            .iter()
            .filter(|endpoint| !endpoints_after.contains(endpoint))
            .copied()
            .collect();

        Some(InterviewDiff {
            node: node.data.clone(),
            changed_attributes: changed,
            removed_attributes: removed_paths,
            endpoints_added,
            endpoints_removed,
        })
    }

    /// Merge fresh attribute values without treating them as an interview.
    ///
    /// Polling produces the same diff an interview does, but must not bump
    /// `interview_version` or `last_interview`: those describe an explicit
    /// re-interview, and clients use them to decide whether to rebuild their
    /// view of the node.
    ///
    /// `coverage` decides what an *absent* path means, and getting it wrong is
    /// not a subtle failure — see [`Coverage`].
    pub fn merge_attributes(
        &self,
        node_id: u64,
        attributes: AttributesData,
        coverage: Coverage,
    ) -> Option<InterviewDiff> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&node_id)?;

        let endpoints_before = node.data.endpoints();
        let mut changed = Vec::new();
        for (path, value) in &attributes {
            if node.data.attributes.get(path) != Some(value) {
                changed.push((path.clone(), value.clone()));
            }
        }
        let removed: Vec<String> = match coverage {
            Coverage::Complete => node
                .data
                .attributes
                .keys()
                .filter(|path| !attributes.contains_key(*path))
                .cloned()
                .collect(),
            // Nothing can be concluded from a path a delta does not mention.
            Coverage::Partial => Vec::new(),
        };

        match coverage {
            Coverage::Complete => node.data.attributes = attributes,
            Coverage::Partial => node.data.attributes.extend(attributes),
        }
        node.data.is_bridge = detect_bridge(&node.data.attributes);
        node.data.matter_version = detect_matter_version(&node.data.attributes);

        let endpoints_after = node.data.endpoints();
        Some(InterviewDiff {
            endpoints_added: endpoints_after
                .iter()
                .filter(|endpoint| !endpoints_before.contains(endpoint))
                .copied()
                .collect(),
            endpoints_removed: endpoints_before
                .iter()
                .filter(|endpoint| !endpoints_after.contains(endpoint))
                .copied()
                .collect(),
            node: node.data.clone(),
            changed_attributes: changed,
            removed_attributes: removed,
        })
    }

    /// Record a single attribute value, reporting the node only when the value
    /// actually changed.
    pub fn set_attribute(&self, node_id: u64, path: &str, value: Value) -> Option<MatterNodeData> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&node_id)?;
        if node.data.attributes.get(path) == Some(&value) {
            return None;
        }
        node.data.attributes.insert(path.to_string(), value);
        Some(node.data.clone())
    }
}

/// Whether a set of attribute values is everything the node has, or only what
/// just changed.
///
/// The distinction is the whole meaning of an absent path. A poll is a
/// wildcard read, so a path it does not carry is a path the node no longer
/// has. A subscription report carries only what changed, so a path it does
/// not carry says nothing at all.
///
/// Reading one as the other is destructive rather than merely inaccurate:
/// applying a device's first report as though it were complete empties the
/// store down to the handful of attributes that happened to change, and
/// announces every endpoint that reported nothing — endpoint 0 included — as
/// removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// Every attribute the node has. An absent path is a removal.
    Complete,
    /// Only what changed. An absent path means nothing.
    Partial,
}

/// What changed when interview results were merged.
#[derive(Clone, Debug)]
pub struct InterviewDiff {
    pub node: MatterNodeData,
    pub changed_attributes: Vec<(String, Value)>,
    pub removed_attributes: Vec<String>,
    pub endpoints_added: Vec<u16>,
    pub endpoints_removed: Vec<u16>,
}

/// A bridge exposes the Aggregator device type (0x000E) in some endpoint's
/// Descriptor DeviceTypeList (cluster 29, attribute 0).
fn detect_bridge(attributes: &AttributesData) -> bool {
    const AGGREGATOR_DEVICE_TYPE: u64 = 0x000E;
    attributes.iter().any(|(path, value)| {
        let is_device_type_list = path
            .split('/')
            .nth(1)
            .zip(path.split('/').nth(2))
            .map(|(cluster, attribute)| cluster == "29" && attribute == "0")
            .unwrap_or(false);
        is_device_type_list
            && value
                .as_array()
                .map(|entries| {
                    entries.iter().any(|entry| {
                        entry
                            .get("deviceType")
                            .or_else(|| entry.get("0"))
                            .and_then(Value::as_u64)
                            == Some(AGGREGATOR_DEVICE_TYPE)
                    })
                })
                .unwrap_or(false)
    })
}

/// The Matter specification version a node implements.
///
/// `BasicInformation::SpecificationVersion` (0x15) is authoritative, but only
/// devices from 1.3 onwards report it; older ones are estimated from
/// `DataModelRevision` (0x00). The estimate deliberately stops at revision 17
/// rather than guessing: a later revision maps to no single spec version, and
/// reporting nothing is better than reporting the wrong one.
fn detect_matter_version(attributes: &AttributesData) -> Option<String> {
    if let Some(raw) = attributes
        .get("0/40/21")
        .and_then(Value::as_u64)
        .filter(|raw| *raw > 0)
    {
        // uint32, most significant byte first: major, minor, patch, reserved.
        let major = (raw >> 24) & 0xFF;
        let minor = (raw >> 16) & 0xFF;
        let patch = (raw >> 8) & 0xFF;
        return Some(format!("{}.{}.{}", major, minor, patch));
    }
    match attributes.get("0/40/0").and_then(Value::as_u64) {
        Some(revision) if revision <= 16 => Some("<1.2.0".into()),
        Some(17) => Some("1.2.0".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_with_node(node_id: u64) -> NodeStore {
        let store = NodeStore::new();
        store.upsert(StoredNode::new(MatterNodeData::new(
            node_id,
            "2026-01-01T00:00:00.000Z".into(),
        )));
        store
    }

    #[test]
    fn interview_reports_changed_attributes_and_new_endpoints() {
        let store = store_with_node(1);
        let mut attributes = AttributesData::new();
        attributes.insert("0/40/1".into(), json!("ACME"));
        attributes.insert("1/6/0".into(), json!(false));

        let diff = store
            .apply_interview(1, attributes.clone(), "2026-01-02T00:00:00.000Z".into())
            .unwrap();
        assert_eq!(diff.changed_attributes.len(), 2);
        assert_eq!(diff.endpoints_added, vec![0, 1]);
        assert_eq!(diff.node.interview_version, 1);

        // A second identical interview changes nothing but bumps the version.
        let diff = store
            .apply_interview(1, attributes, "2026-01-03T00:00:00.000Z".into())
            .unwrap();
        assert!(diff.changed_attributes.is_empty());
        assert!(diff.endpoints_added.is_empty());
        assert_eq!(diff.node.interview_version, 2);
    }

    #[test]
    fn interview_reports_disappearing_endpoints() {
        let store = store_with_node(1);
        let mut attributes = AttributesData::new();
        attributes.insert("1/6/0".into(), json!(true));
        attributes.insert("2/6/0".into(), json!(true));
        store.apply_interview(1, attributes, "t1".into()).unwrap();

        let mut fewer = AttributesData::new();
        fewer.insert("1/6/0".into(), json!(true));
        let diff = store.apply_interview(1, fewer, "t2".into()).unwrap();
        assert_eq!(diff.endpoints_removed, vec![2]);
        assert_eq!(diff.removed_attributes, vec!["2/6/0".to_string()]);
    }

    #[test]
    fn polling_diffs_attributes_without_bumping_the_interview_version() {
        let store = store_with_node(1);
        let mut attributes = AttributesData::new();
        attributes.insert("1/6/0".into(), json!(false));
        store
            .apply_interview(1, attributes.clone(), "t1".into())
            .unwrap();

        attributes.insert("1/6/0".into(), json!(true));
        let diff = store
            .merge_attributes(1, attributes, Coverage::Complete)
            .unwrap();
        assert_eq!(diff.changed_attributes.len(), 1);
        assert_eq!(diff.changed_attributes[0].0, "1/6/0");
        // A poll is not an interview.
        assert_eq!(diff.node.interview_version, 1);
        assert_eq!(diff.node.last_interview, "t1");
    }

    #[test]
    fn setting_an_unchanged_attribute_reports_no_update() {
        let store = store_with_node(1);
        assert!(store.set_attribute(1, "1/6/0", json!(true)).is_some());
        assert!(store.set_attribute(1, "1/6/0", json!(true)).is_none());
        assert!(store.set_attribute(1, "1/6/0", json!(false)).is_some());
    }

    #[test]
    fn availability_transitions_are_reported_once() {
        let store = store_with_node(1);
        assert!(
            store.set_available(1, false).unwrap().1,
            "the first change is reported"
        );
        assert!(
            !store.set_available(1, false).unwrap().1,
            "setting the same value again is not a change"
        );
    }

    #[test]
    fn bridges_are_detected_from_the_descriptor_device_type_list() {
        let store = store_with_node(1);
        let mut attributes = AttributesData::new();
        attributes.insert(
            "1/29/0".into(),
            json!([{ "deviceType": 14, "revision": 1 }]),
        );
        let diff = store.apply_interview(1, attributes, "t".into()).unwrap();
        assert!(diff.node.is_bridge);
    }

    #[test]
    fn matter_version_prefers_the_specification_version_attribute() {
        let store = store_with_node(1);
        let mut attributes = AttributesData::new();
        // SpecificationVersion is major/minor/patch/reserved, one byte each,
        // most significant first: 1.4.0 is 0x01040000.
        attributes.insert("0/40/21".into(), json!(0x0104_0000u64));
        attributes.insert("0/40/0".into(), json!(16));
        let diff = store.apply_interview(1, attributes, "t".into()).unwrap();
        assert_eq!(diff.node.matter_version.as_deref(), Some("1.4.0"));
    }

    #[test]
    fn matter_version_falls_back_to_the_data_model_revision() {
        let cases = [
            (json!(0x0103_0000u64), json!(17), "1.3.0"),
            (Value::Null, json!(17), "1.2.0"),
            (Value::Null, json!(16), "<1.2.0"),
            (Value::Null, json!(1), "<1.2.0"),
            // A zero specification version is "not reported", not 0.0.0.
            (json!(0), json!(17), "1.2.0"),
        ];
        for (specification, revision, expected) in cases {
            let store = store_with_node(1);
            let mut attributes = AttributesData::new();
            if !specification.is_null() {
                attributes.insert("0/40/21".into(), specification.clone());
            }
            attributes.insert("0/40/0".into(), revision.clone());
            let diff = store.apply_interview(1, attributes, "t".into()).unwrap();
            assert_eq!(
                diff.node.matter_version.as_deref(),
                Some(expected),
                "specification={:?} revision={:?}",
                specification,
                revision
            );
        }
    }

    #[test]
    fn an_unmappable_data_model_revision_reports_nothing() {
        let store = store_with_node(1);
        let mut attributes = AttributesData::new();
        // Newer than anything this build can map, with no specification
        // version to fall back on.
        attributes.insert("0/40/0".into(), json!(18));
        let diff = store.apply_interview(1, attributes, "t".into()).unwrap();
        assert_eq!(diff.node.matter_version, None);
    }

    #[test]
    fn ip_addresses_stay_out_of_the_wire_model() {
        let store = store_with_node(1);
        store.set_ip_addresses(1, vec!["fd00::1".into()]);
        let wire = serde_json::to_value(store.get(1).unwrap()).unwrap();
        assert!(wire.get("ip_addresses").is_none());
        assert_eq!(store.ip_addresses(1), vec!["fd00::1".to_string()]);
    }

    #[test]
    fn snapshots_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.json");
        {
            let store = NodeStore::load(&path).unwrap();
            let mut node = StoredNode::new(MatterNodeData::new(5, "t".into()));
            node.ip_addresses = vec!["fd00::5".into()];
            store.upsert(node);
            store.save().unwrap();
        }
        let reloaded = NodeStore::load(&path).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.ip_addresses(5), vec!["fd00::5".to_string()]);
        assert_eq!(reloaded.highest_node_id(), Some(5));
    }

    #[test]
    fn test_nodes_do_not_advance_the_id_counter() {
        let store = NodeStore::new();
        store.upsert(StoredNode::new(MatterNodeData::new(
            crate::protocol::model::TEST_NODE_START + 1,
            "t".into(),
        )));
        assert_eq!(store.highest_node_id(), None);
        store.upsert(StoredNode::new(MatterNodeData::new(3, "t".into())));
        assert_eq!(store.highest_node_id(), Some(3));
    }
}
