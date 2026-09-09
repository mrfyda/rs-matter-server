//! Importing a matterjs-server installation.
//!
//! The point of this module is that a user switching servers does not have to
//! re-commission anything. A Matter device recognises its fabric by the root
//! public key and grants administrative access to one controller node id, so a
//! controller that presents the same root, the same operational certificate
//! and the same key *is* the controller the device already trusts. matter.js
//! persists all three, so the fabric can be moved across whole.
//!
//! What moves:
//!
//! | matter.js | here |
//! |---|---|
//! | `Fabric.Config` (root cert, NOC, ICAC, operational key, IPK) | the rs-matter fabric |
//! | the CA's root or intermediate private key | `controller-icac-key.bin` |
//! | `commissionedNodes` and the per-node commissioning state | `nodes.json` |
//! | the `config` namespace | `config.json` |
//!
//! What does not, and why it does not matter: sessions and CASE resumption
//! records (a fresh handshake replaces them), subscriptions (this server
//! polls), and the cached attribute values — matter.js stores those decoded
//! into its own object model, and the device is a better source anyway, so the
//! first poll after startup fills them in.
//!
//! The source directory is only ever read. A user who wants to go back to
//! matterjs-server points it at the same directory and finds it untouched.

pub mod model;
pub mod store;
pub mod value;

use std::path::Path;

use anyhow::{Context, Result};

pub use model::{ImportedConfig, ImportedFabric, ImportedNode};
pub use store::MatterJsStorage;

use crate::protocol::model::MatterNodeData;
use crate::storage::{ConfigStore, NodeStore, StoredNode};

/// Everything read out of a matterjs-server storage directory.
#[derive(Debug)]
pub struct Import {
    pub fabric: ImportedFabric,
    pub nodes: Vec<ImportedNode>,
    pub config: ImportedConfig,
    /// The namespace the fabric came from, for the log line that reports it.
    pub namespace: String,
}

/// Read a matterjs-server storage directory.
///
/// Reading is separated from writing so that a source that cannot be imported
/// fails before this server has created a fabric of its own — the difference
/// between "fix the path and try again" and "wipe the storage directory and
/// try again".
pub fn read(source: &Path, namespace: Option<&str>) -> Result<Import> {
    let storage = MatterJsStorage::open(source)?;
    log::info!(
        "Reading matterjs-server storage at {} (namespaces: {})",
        source.display(),
        storage.namespace_names().join(", ")
    );

    let controller = model::find_controller_namespace(&storage, namespace)?;
    let fabric = model::read_fabric(controller)
        .with_context(|| format!("reading the fabric from '{}'", controller.name))?;
    let nodes = model::read_nodes(controller);
    let config = model::read_config(&storage)?;

    log::info!(
        "Found a fabric with {} commissioned node(s) in '{}' ({:?} storage)",
        nodes.len(),
        controller.name,
        controller.driver
    );

    Ok(Import {
        fabric,
        nodes,
        config,
        namespace: controller.name.clone(),
    })
}

impl Import {
    /// What this import would do, for `--import-matterjs-dry-run`.
    ///
    /// A migration is a one-way step taken against a working installation, so
    /// there is a way to look before leaping — and this is also the first
    /// thing worth asking for when someone reports that an import went wrong.
    pub fn summary(&self) -> String {
        use std::fmt::Write;

        // One column width for every label, so the report reads as a table.
        let mut out = String::new();
        // `row("")` indents a continuation line under the previous label.
        let mut row = |label: &str, value: String| {
            let label = if label.is_empty() {
                String::new()
            } else {
                format!("{}:", label)
            };
            let _ = writeln!(out, "{:<24}{}", label, value);
        };

        row("Namespace", self.namespace.clone());
        row("Fabric id", format!("0x{:016x}", self.fabric.fabric_id));
        row(
            "Controller node id",
            format!("0x{:016x} ({})", self.fabric.node_id, self.fabric.node_id),
        );
        row("Vendor id", format!("0x{:04x}", self.fabric.vendor_id));
        row(
            "Fabric label",
            if self.fabric.label.is_empty() {
                "(none)".to_string()
            } else {
                self.fabric.label.clone()
            },
        );
        row(
            "Device NOCs signed by",
            if self.fabric.issuer_is_icac {
                "the intermediate CA".to_string()
            } else {
                "the root CA".to_string()
            },
        );
        row(
            "Compressed fabric id",
            match self.fabric.compressed_fabric_id {
                Some(id) => format!("0x{:016x} (checked on import)", id),
                None => "not stored by the source (derived here)".to_string(),
            },
        );
        row("Nodes", self.nodes.len().to_string());
        for node in &self.nodes {
            row(
                "",
                format!(
                    "{} — {} address(es){}",
                    node.node_id,
                    node.addresses.len(),
                    match node.fabric_index_on_peer {
                        Some(index) => format!(", fabric index {} on the device", index),
                        None => String::new(),
                    }
                ),
            );
        }
        row(
            "Wi-Fi credentials",
            summarise(
                self.config
                    .wifi
                    .iter()
                    .map(|(id, ssid, _)| format!("{} ({})", id, ssid)),
            ),
        );
        row(
            "Thread datasets",
            summarise(self.config.thread.iter().map(|(id, _)| id.clone())),
        );
        row(
            "Next node id",
            match self.config.next_node_id {
                Some(next) => next.to_string(),
                None => "(not stored)".into(),
            },
        );

        out.push_str(
            "\nAttributes are not copied: each node is read from the device on its first \
             poll after startup.\n",
        );
        out
    }
}

fn summarise(items: impl Iterator<Item = String>) -> String {
    let listed: Vec<String> = items.collect();
    if listed.is_empty() {
        "none".to_string()
    } else {
        listed.join(", ")
    }
}

/// Write the node list and settings that go with an imported fabric.
///
/// Called only after the fabric was actually installed, so this cannot run
/// against a server that already had state of its own.
pub fn apply_state(import: &Import, storage_path: &Path) -> Result<()> {
    let nodes = NodeStore::load(storage_path.join("nodes.json"))?;
    for node in &import.nodes {
        let mut stored = StoredNode::new(MatterNodeData::new(
            node.node_id,
            commissioned_at(node.commissioned_at),
        ));
        // Nothing has been read from the device yet, and claiming otherwise
        // would have clients build a view of a node from an empty attribute
        // set. The monitor's first poll flips this and publishes the node.
        stored.data.available = false;
        stored.ip_addresses = node.addresses.clone();
        stored.device_fabric_index = node.fabric_index_on_peer;
        nodes.upsert(stored);
    }
    nodes.save()?;

    let config = ConfigStore::load(storage_path.join("config.json"))?;
    if let Some(label) = &import.config.fabric_label {
        config.set_fabric_label(label)?;
    }
    // matter.js hands out the *next* id; ours reserves above the last used.
    if let Some(next) = import.config.next_node_id {
        config.reserve_node_ids_above(next.saturating_sub(1))?;
    }
    for node in &import.nodes {
        config.reserve_node_ids_above(node.node_id)?;
    }

    for (id, ssid, credentials) in &import.config.wifi {
        config
            .set_wifi_credentials(Some(id), ssid, Some(credentials))
            .map_err(|error| {
                anyhow::anyhow!("importing the '{}' Wi-Fi credentials: {}", id, error)
            })?;
    }
    for (id, dataset) in &import.config.thread {
        config
            .set_thread_dataset(Some(id), dataset)
            .map_err(|error| anyhow::anyhow!("importing the '{}' Thread dataset: {}", id, error))?;
    }

    log::info!(
        "Imported {} node(s), {} Wi-Fi and {} Thread credential set(s) from '{}'",
        import.nodes.len(),
        import.config.wifi.len(),
        import.config.thread.len(),
        import.namespace
    );
    if !import.nodes.is_empty() {
        log::info!(
            "Imported nodes report as unavailable until each answers its first poll, which \
             is also when their attributes are read back from the devices"
        );
    }

    Ok(())
}

/// matter.js records a millisecond timestamp; the protocol carries an ISO
/// string. A date that cannot be represented is not worth failing an import
/// over — it is shown in diagnostics and nothing more — so it falls back to
/// now, which is what a freshly commissioned node would carry.
fn commissioned_at(millis: Option<u64>) -> String {
    use chrono::TimeZone;

    millis
        .and_then(|millis| i64::try_from(millis).ok())
        .and_then(|millis| chrono::Utc.timestamp_millis_opt(millis).single())
        // The same spelling `now_iso` produces, so an imported date and a
        // freshly commissioned one are indistinguishable on the wire.
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(crate::api::now_iso)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commissioning_date_survives_the_move() {
        assert_eq!(
            commissioned_at(Some(1_700_000_000_000)),
            "2023-11-14T22:13:20.000Z"
        );
    }

    #[test]
    fn the_dry_run_summary_names_what_would_move() {
        let import = Import {
            fabric: ImportedFabric {
                root_cert: vec![1],
                noc: vec![2],
                icac: Vec::new(),
                operational_key: vec![0; 32],
                ipk_epoch_key: vec![0; 16],
                vendor_id: 0xFFF1,
                node_id: 112233,
                fabric_id: 1,
                fabric_index: 1,
                label: "Living Room".into(),
                issuer_key: vec![0; 32],
                issuer_is_icac: false,
                compressed_fabric_id: Some(0xdead_beef),
            },
            nodes: vec![ImportedNode {
                node_id: 4,
                commissioned_at: None,
                fabric_index_on_peer: Some(2),
                addresses: vec!["fd11::1".into()],
            }],
            config: ImportedConfig {
                fabric_label: Some("Living Room".into()),
                next_node_id: Some(9),
                wifi: vec![("default".into(), "home".into(), "secret".into())],
                thread: Vec::new(),
            },
            namespace: "server".into(),
        };

        let summary = import.summary();
        assert!(summary.contains("0x000000000001b669"), "{}", summary);
        assert!(summary.contains("Living Room"), "{}", summary);
        assert!(summary.contains("root CA"), "{}", summary);
        assert!(
            summary.contains("fabric index 2 on the device"),
            "{}",
            summary
        );
        assert!(summary.contains("default (home)"), "{}", summary);
        assert!(summary.contains("Thread datasets"), "{}", summary);
        assert!(summary.contains("none"), "{}", summary);
        assert!(
            !summary.contains("secret"),
            "a summary printed to a terminal must not carry passwords: {}",
            summary
        );
    }

    #[test]
    fn a_missing_date_falls_back_to_now_rather_than_failing() {
        let now = commissioned_at(None);
        assert!(now.ends_with('Z') && now.len() == 24, "{}", now);
    }
}
