//! Attribute monitoring.
//!
//! The protocol promises `attribute_updated` events, and clients build their
//! whole view of a device from them. rs-matter 0.3 has no client-side
//! subscription receiver — establishing a subscription is supported, but the
//! ongoing reports arrive as device-initiated exchanges that a controller has
//! no API to consume — so changes are discovered by polling instead.
//!
//! The observable protocol behaviour is the same: a change produces an
//! `attribute_updated` event, a node that stops answering produces a
//! `node_updated` with `available: false`, and endpoints that come and go
//! produce endpoint events. Only the transport differs, and this module is the
//! single place that has to change when subscriptions become available.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::ServerContext;
use crate::protocol::events::Event;

/// How the monitor paces itself.
#[derive(Clone, Copy, Debug)]
pub struct MonitorConfig {
    /// How often a reachable node is re-read.
    pub interval: Duration,
    /// How long to wait before retrying a node that did not answer. Sleepy and
    /// unplugged devices are the common case, and hammering them wastes radio
    /// time that reachable nodes need.
    pub offline_interval: Duration,
    /// Pause between individual node polls, so a large fabric does not produce
    /// a burst of traffic every cycle.
    pub stagger: Duration,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            offline_interval: Duration::from_secs(300),
            stagger: Duration::from_millis(250),
        }
    }
}

/// Poll every known node forever.
pub async fn run(context: Arc<ServerContext>, config: MonitorConfig) {
    let mut last_polled: BTreeMap<u64, Instant> = BTreeMap::new();

    loop {
        for node in context.nodes.all() {
            // Imported test nodes have no device behind them.
            if node.is_test_node() {
                continue;
            }

            let due = match last_polled.get(&node.node_id) {
                Some(last) => {
                    let interval = if node.available {
                        config.interval
                    } else {
                        config.offline_interval
                    };
                    last.elapsed() >= interval
                }
                None => true,
            };
            if !due {
                continue;
            }

            last_polled.insert(node.node_id, Instant::now());
            poll_node(&context, node.node_id).await;
            async_io::Timer::after(config.stagger).await;
        }

        // Forget nodes that have been removed, so the map cannot grow without
        // bound over a long run.
        last_polled.retain(|node_id, _| context.nodes.contains(*node_id));
        async_io::Timer::after(config.stagger.max(Duration::from_secs(1))).await;
    }
}

/// Read one node and publish whatever changed.
pub async fn poll_node(context: &Arc<ServerContext>, node_id: u64) {
    let paths = crate::matter::interaction::interview_paths();
    match context.matter.read_attributes(node_id, paths, false).await {
        Ok(attributes) => {
            if attributes.is_empty() {
                return;
            }
            publish_changes(context, node_id, attributes);
            mark_available(context, node_id, true);
        }
        Err(error) => {
            log::debug!("Node {} did not answer a poll: {}", node_id, error);
            mark_available(context, node_id, false);
        }
    }
}

fn publish_changes(
    context: &Arc<ServerContext>,
    node_id: u64,
    attributes: crate::protocol::model::AttributesData,
) {
    let Some(diff) = context.nodes.merge_attributes(node_id, attributes) else {
        return;
    };
    if diff.changed_attributes.is_empty()
        && diff.endpoints_added.is_empty()
        && diff.endpoints_removed.is_empty()
    {
        return;
    }

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
    if let Err(error) = context.nodes.save() {
        log::warn!("Could not persist polled attributes: {}", error);
    }
}

/// Record reachability, announcing only a real transition.
fn mark_available(context: &Arc<ServerContext>, node_id: u64, available: bool) {
    let Some((node, changed)) = context.nodes.set_available(node_id, available) else {
        return;
    };
    if !changed {
        return;
    }
    log::info!(
        "Node {} is now {}",
        node_id,
        if available {
            "available"
        } else {
            "unavailable"
        }
    );
    let _ = context.nodes.save();
    context.events.publish(Event::node_updated(&node));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::test_context;
    use crate::protocol::model::{AttributesData, MatterNodeData};
    use crate::storage::StoredNode;
    use serde_json::json;

    fn context_with_node() -> Arc<ServerContext> {
        let context = test_context();
        context.nodes.upsert(StoredNode::new(MatterNodeData::new(
            1,
            "2026-01-01T00:00:00.000Z".into(),
        )));
        Arc::new(context)
    }

    #[test]
    fn a_changed_attribute_produces_an_event() {
        let context = context_with_node();
        let events = context.events.subscribe();

        let mut attributes = AttributesData::new();
        attributes.insert("1/6/0".into(), json!(true));
        publish_changes(&context, 1, attributes.clone());

        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event.name().to_string());
        }
        assert_eq!(
            seen,
            vec!["endpoint_added", "attribute_updated", "node_updated"]
        );

        // Polling again with the same values is silent.
        publish_changes(&context, 1, attributes);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn availability_transitions_are_announced_once() {
        let context = context_with_node();
        let events = context.events.subscribe();

        mark_available(&context, 1, false);
        let event = events.try_recv().unwrap();
        assert_eq!(event.name(), "node_updated");
        assert_eq!(event.payload["data"]["available"], json!(false));

        // No further event while it stays unavailable.
        mark_available(&context, 1, false);
        assert!(events.try_recv().is_err());

        mark_available(&context, 1, true);
        assert_eq!(
            events.try_recv().unwrap().payload["data"]["available"],
            json!(true)
        );
    }

    #[test]
    fn polling_an_unknown_node_is_harmless() {
        let context = context_with_node();
        let events = context.events.subscribe();
        mark_available(&context, 99, true);
        publish_changes(&context, 99, AttributesData::new());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn offline_nodes_are_polled_less_often() {
        let config = MonitorConfig::default();
        assert!(config.offline_interval > config.interval);
    }
}
