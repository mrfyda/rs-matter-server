//! Node (device) state management.
//!
//! Tracks commissioned Matter devices and their attribute values.
//! State is kept in memory and optionally persisted to disk.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// In-memory store of commissioned nodes.
#[derive(Default)]
pub struct NodeStore {
    nodes: std::sync::Mutex<Vec<NodeInfo>>,
    storage_path: Option<PathBuf>,
}

/// Node info as stored in the server state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    /// The device's NodeID on our fabric.
    pub node_id: u64,
    /// When this device was commissioned.
    pub date_commissioned: String,
    /// Last successful interview (attribute discovery).
    pub last_interview: Option<String>,
    /// Interview schema version counter.
    pub interview_version: u64,
    /// Is the device currently reachable?
    pub available: bool,
    /// Is this device a bridge (has sub-devices)?
    pub is_bridge: bool,
    /// Raw attribute values from last interview.
    #[serde(default)]
    pub attributes: Value,
    /// Active subscriptions.
    #[serde(default)]
    pub attribute_subscriptions: Vec<Value>,
    /// Last known operational addresses, persisted for diagnostics and
    /// callers that prefer cached resolution.
    #[serde(default)]
    pub ip_addresses: Vec<String>,
}

impl NodeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a store backed by a JSON snapshot on disk. Missing state is an
    /// empty store; malformed state is reported rather than silently erased.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let nodes = if path.exists() {
            let bytes = fs::read(&path)
                .with_context(|| format!("reading node state {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing node state {}", path.display()))?
        } else {
            Vec::new()
        };
        Ok(Self {
            nodes: std::sync::Mutex::new(nodes),
            storage_path: Some(path),
        })
    }

    /// Persist the current snapshot. The temporary file plus rename prevents
    /// a process exit during a write from leaving a truncated state file.
    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.storage_path else {
            return Ok(());
        };
        let nodes = self.nodes.lock().unwrap().clone();
        let bytes = serde_json::to_vec_pretty(&nodes).context("serializing node state")?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, bytes).with_context(|| format!("writing node state {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("installing node state {}", path.display()))?;
        Ok(())
    }

    /// Add a newly commissioned node.
    pub fn add_node(&self, node: NodeInfo) {
        let mut nodes = self.nodes.lock().unwrap();
        if let Some(existing) = nodes.iter_mut().find(|n| n.node_id == node.node_id) {
            *existing = node;
        } else {
            nodes.push(node);
        }
    }

    /// Remove a node by NodeID.
    pub fn remove_node(&self, node_id: u64) -> bool {
        let mut nodes = self.nodes.lock().unwrap();
        let len_before = nodes.len();
        nodes.retain(|n| n.node_id != node_id);
        nodes.len() < len_before
    }

    /// Get all nodes.
    pub fn get_all(&self) -> Vec<NodeInfo> {
        self.nodes.lock().unwrap().clone()
    }

    /// Get all nodes optionally filtered by availability.
    pub fn get_all_filtered(&self, only_available: bool) -> Vec<NodeInfo> {
        let nodes = self.nodes.lock().unwrap();
        nodes
            .iter()
            .filter(|n| !only_available || n.available)
            .cloned()
            .collect()
    }

    /// Get a node by NodeID.
    pub fn get(&self, node_id: u64) -> Option<NodeInfo> {
        self.nodes
            .lock()
            .unwrap()
            .iter()
            .find(|n| n.node_id == node_id)
            .cloned()
    }

    /// Update a node's availability status.
    pub fn set_available(&self, node_id: u64, available: bool) {
        let mut nodes = self.nodes.lock().unwrap();
        if let Some(n) = nodes.iter_mut().find(|n| n.node_id == node_id) {
            n.available = available;
        }
    }
}
