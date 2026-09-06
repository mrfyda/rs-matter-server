//! Restart recovery.
//!
//! Everything the protocol promises to survive a restart is written through a
//! temporary file and reloaded here, because the failure mode this guards
//! against — a controller that comes back with no nodes — means every device
//! has to be commissioned again.

use rs_matter_server::protocol::model::MatterNodeData;
use rs_matter_server::storage::{ConfigStore, NodeStore, StoredNode};

fn commissioned_node(node_id: u64) -> StoredNode {
    let mut node = MatterNodeData::new(node_id, "2026-01-01T00:00:00.000Z".into());
    node.attributes
        .insert("0/40/1".into(), serde_json::json!("ACME"));
    node.attributes
        .insert("1/6/0".into(), serde_json::json!(true));
    node.interview_version = 3;
    let mut stored = StoredNode::new(node);
    stored.ip_addresses = vec!["fd00::5".into()];
    stored.device_fabric_index = Some(2);
    stored
}

#[test]
fn commissioned_nodes_come_back_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nodes.json");

    {
        let nodes = NodeStore::load(&path).unwrap();
        nodes.upsert(commissioned_node(1));
        nodes.upsert(commissioned_node(2));
        nodes.save().unwrap();
    }

    let nodes = NodeStore::load(&path).unwrap();
    assert_eq!(nodes.len(), 2);
    let node = nodes.get(1).unwrap();
    assert_eq!(node.interview_version, 3);
    assert_eq!(node.attributes["0/40/1"], serde_json::json!("ACME"));
    // Controller-internal state survives too: without it a node cannot be
    // decommissioned cleanly.
    assert_eq!(nodes.ip_addresses(1), vec!["fd00::5".to_string()]);
    assert_eq!(nodes.get_stored(1).unwrap().device_fabric_index, Some(2));
}

#[test]
fn node_ids_are_never_reissued_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");

    let first = {
        let config = ConfigStore::load(&config_path).unwrap();
        (
            config.allocate_node_id().unwrap(),
            config.allocate_node_id().unwrap(),
        )
    };
    assert_eq!(first, (1, 2));

    let config = ConfigStore::load(&config_path).unwrap();
    assert_eq!(config.allocate_node_id().unwrap(), 3);
}

#[test]
fn a_snapshot_from_an_older_build_pushes_the_counter_past_its_ids() {
    let dir = tempfile::tempdir().unwrap();
    let nodes = NodeStore::load(dir.path().join("nodes.json")).unwrap();
    nodes.upsert(commissioned_node(9));
    let config = ConfigStore::load(dir.path().join("config.json")).unwrap();

    // This is what startup does when the node snapshot is ahead of the counter.
    config
        .reserve_node_ids_above(nodes.highest_node_id().unwrap())
        .unwrap();
    assert_eq!(config.allocate_node_id().unwrap(), 10);
}

#[test]
fn credentials_and_the_fabric_label_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");

    {
        let config = ConfigStore::load(&path).unwrap();
        config
            .set_wifi_credentials(None, "home", Some("hunter2"))
            .unwrap();
        config
            .set_wifi_credentials(Some("Guest"), "guest-net", Some("visitor"))
            .unwrap();
        config.set_thread_dataset(None, "0208112233445566").unwrap();
        config.set_fabric_label("Living Room").unwrap();
    }

    let config = ConfigStore::load(&path).unwrap();
    assert_eq!(config.fabric_label(), "Living Room");
    assert!(config.wifi_credentials_set());
    assert!(config.thread_credentials_set());
    assert_eq!(
        config.wifi_credentials(Some("Guest")),
        Some(("guest-net".to_string(), "visitor".to_string()))
    );
    // The reloaded summary still hides the secrets.
    let rendered = serde_json::to_string(&config.summaries()).unwrap();
    assert!(rendered.contains("guest-net"));
    assert!(!rendered.contains("visitor"));
}

#[test]
fn an_interrupted_write_cannot_truncate_existing_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nodes.json");
    let nodes = NodeStore::load(&path).unwrap();
    nodes.upsert(commissioned_node(1));
    nodes.save().unwrap();

    // A leftover temporary file from a previous interrupted save must not
    // affect the committed snapshot.
    std::fs::write(path.with_extension("json.tmp"), b"garbage").unwrap();
    let reloaded = NodeStore::load(&path).unwrap();
    assert_eq!(reloaded.len(), 1);
}

#[test]
fn missing_state_files_start_empty_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let nodes = NodeStore::load(dir.path().join("absent.json")).unwrap();
    assert!(nodes.is_empty());
    let config = ConfigStore::load(dir.path().join("absent-config.json")).unwrap();
    assert_eq!(config.fabric_label(), "HomeAssistant");
}

#[test]
fn corrupt_state_is_reported_rather_than_silently_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nodes.json");
    std::fs::write(&path, b"{ not json").unwrap();
    assert!(NodeStore::load(&path).is_err());
}
