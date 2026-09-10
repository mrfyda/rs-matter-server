//! Attribute monitoring.
//!
//! The protocol promises `attribute_updated` events, and clients build their
//! whole view of a device from them. A device that reports its own changes
//! produces them the moment they happen, so this loop's first job is to get
//! every node subscribed — and its second is to be the fallback for nodes that
//! will not subscribe, and the watchdog for subscriptions that go quiet.
//!
//! What it does for one node, in order:
//!
//! * **Reporting already?** Nothing to do. The device is pushing changes and
//!   `crate::matter::reports` is turning them into events.
//! * **Not subscribed?** Subscribe. The priming report that comes back is a
//!   full snapshot, so this is also the read that would otherwise have been
//!   done — one transaction, not two.
//! * **Would not subscribe?** Poll it, exactly as before. A device with no
//!   subscription slots left, or one that refuses a wildcard, still produces
//!   the same events; only the latency differs.
//! * **Went quiet?** A device owes a report every `max_interval` even when
//!   nothing changes, so silence for twice that long means the subscription is
//!   gone. It is forgotten and re-established from scratch.
//!
//! The observable protocol behaviour is identical either way: a change
//! produces an `attribute_updated` event, a node that stops answering produces
//! a `node_updated` with `available: false`, and endpoints that come and go
//! produce endpoint events.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::ServerContext;
use crate::protocol::error::{ApiError, ErrorCode as ProtocolError};
use crate::protocol::events::Event;

/// How soon a device may report a change: immediately.
///
/// Matter's floor is a rate limit, not a delay — a device may not report more
/// often than this. Zero is what a controller that wants a button press
/// reflected at once asks for, and it is what the reference asks for too.
const MIN_REPORT_INTERVAL_SECS: u16 = 0;

/// How long a device may stay silent before it owes a report anyway.
///
/// This is the subscription's keepalive, not its change latency: a change is
/// reported when it happens. Five minutes is long enough that an idle device
/// (and a sleepy one) is not woken for nothing, and short enough that a
/// subscription which died silently is noticed within ten.
const MAX_REPORT_INTERVAL_SECS: u16 = 300;

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

            // A node that is reporting for itself needs nothing from here.
            if context.subscriptions.is_live(node.node_id) {
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
            maintain_node(&context, node.node_id).await;
            async_io::Timer::after(config.stagger).await;
        }

        // Forget nodes that have been removed, so the map cannot grow without
        // bound over a long run.
        last_polled.retain(|node_id, _| context.nodes.contains(*node_id));
        // A subscription that stopped reporting is dropped here rather than
        // where it is noticed, so the next pass treats the node as unsubscribed
        // and starts again.
        for node_id in context.subscriptions.forget_silent() {
            log::info!("Node {} stopped reporting; will re-subscribe", node_id);
        }
        async_io::Timer::after(config.stagger.max(Duration::from_secs(1))).await;
    }
}

/// Bring one node back under a subscription, or fall back to reading it.
///
/// A failure to *reach* the node is not followed by a read: opening the CASE
/// session is the expensive part and it has just failed, and retrying it costs
/// the whole retransmit budget again on an actor that serves one op at a time.
pub async fn maintain_node(context: &Arc<ServerContext>, node_id: u64) {
    // Whatever was recorded is not working, or this would not be running.
    context.subscriptions.forget(node_id);

    let first_read = context.nodes.awaiting_first_interview(node_id);
    match context
        .matter
        .subscribe(node_id, MIN_REPORT_INTERVAL_SECS, MAX_REPORT_INTERVAL_SECS)
        .await
    {
        Ok(subscription) => {
            log::info!(
                "Node {} is reporting on subscription {} (at least every {}s)",
                node_id,
                subscription.subscription_id,
                subscription.max_interval_secs
            );
            context.subscriptions.established(
                node_id,
                subscription.subscription_id,
                Duration::from_secs(subscription.max_interval_secs as u64),
            );
            // The priming report is a full read, so it is published the same
            // way a poll's answer would have been.
            if !subscription.attributes.is_empty() {
                if first_read {
                    publish_first_interview(context, node_id, subscription.attributes);
                } else {
                    publish_changes(context, node_id, subscription.attributes);
                }
            }
            mark_available(context, node_id, true);
        }
        Err(error) if unreachable(&error) => {
            log::debug!("Node {} could not be reached to subscribe: {}", node_id, error);
            mark_available(context, node_id, false);
        }
        Err(error) => {
            log::debug!(
                "Node {} would not subscribe ({}); polling it instead",
                node_id,
                error
            );
            poll_node(context, node_id).await;
        }
    }
}

/// Whether an error means the node could not be reached at all, as opposed to
/// having answered and declined.
fn unreachable(error: &ApiError) -> bool {
    matches!(
        error.code,
        ProtocolError::NodeNotResolving | ProtocolError::NodeNotReady
    )
}

/// Read one node and publish whatever changed.
pub async fn poll_node(context: &Arc<ServerContext>, node_id: u64) {
    // A node this server has never read — one adopted from another server —
    // gets its first read treated as an interview rather than as a change.
    let first_read = context.nodes.awaiting_first_interview(node_id);

    let paths = crate::matter::interaction::interview_paths();
    match context.matter.read_attributes(node_id, paths, false).await {
        Ok(attributes) => {
            if attributes.is_empty() {
                return;
            }
            if first_read {
                publish_first_interview(context, node_id, attributes);
            } else {
                publish_changes(context, node_id, attributes);
            }
            mark_available(context, node_id, true);
        }
        Err(error) => {
            log::debug!("Node {} did not answer a poll: {}", node_id, error);
            mark_available(context, node_id, false);
        }
    }
}

/// Publish the first read of a node as a single complete update.
///
/// Not as the usual stream of per-attribute and per-endpoint events: a client
/// that received this node with no attributes has no endpoints to attach them
/// to, and the reference client drops attribute updates for endpoints it does
/// not know. One `node_updated` carrying the whole node is what rebuilds it —
/// and it is also the honest description of what happened, since the node was
/// interviewed here for the first time.
fn publish_first_interview(
    context: &Arc<ServerContext>,
    node_id: u64,
    attributes: crate::protocol::model::AttributesData,
) {
    let Some(diff) = context
        .nodes
        .apply_interview(node_id, attributes, crate::api::now_iso())
    else {
        return;
    };
    log::info!(
        "Node {} answered for the first time; interviewed {} attribute(s)",
        node_id,
        diff.node.attributes.len()
    );
    if let Err(error) = context.nodes.save() {
        log::warn!("Could not persist the first interview: {}", error);
    }
    context.events.publish(Event::node_updated(&diff.node));
}

pub fn publish_changes(
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

    /// A node that is reporting for itself must not also be polled: the whole
    /// point of subscribing is that the traffic stops.
    #[test]
    fn a_reporting_node_is_left_alone() {
        let context = context_with_node();
        assert!(!context.subscriptions.is_live(1));
        context
            .subscriptions
            .established(1, 42, Duration::from_secs(300));
        assert!(context.subscriptions.is_live(1));
    }

    /// Whether a failure means "could not reach it" — which must not be
    /// followed by a read that would pay the same cost again — or "reached it
    /// and it said no", which polling can still work around.
    #[test]
    fn only_a_session_failure_stops_the_fallback_read() {
        assert!(unreachable(&ApiError::node_not_resolving(1)));
        assert!(unreachable(&ApiError::node_not_ready(1)));
        assert!(!unreachable(&ApiError::sdk("the device refused")));
        assert!(!unreachable(&ApiError::invalid_args("bad path")));
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
    fn a_nodes_first_read_arrives_as_one_complete_update() {
        // An adopted node: known to be on the fabric, never read from.
        let context = test_context();
        let mut imported =
            StoredNode::new(MatterNodeData::new(7, "2023-11-14T22:13:20.000Z".into()));
        imported.data.available = false;
        context.nodes.upsert(imported);
        let context = Arc::new(context);
        let events = context.events.subscribe();

        assert!(context.nodes.awaiting_first_interview(7));

        let mut attributes = AttributesData::new();
        attributes.insert("0/29/0".into(), json!([{ "0": 22, "1": 1 }]));
        attributes.insert("1/6/0".into(), json!(true));
        publish_first_interview(&context, 7, attributes);

        // One event, carrying the whole node: a client that received this node
        // with no attributes has no endpoints for per-attribute events to
        // land on, and rebuilds it from this instead.
        let event = events.try_recv().unwrap();
        assert_eq!(event.name(), "node_updated");
        assert_eq!(event.payload["data"]["node_id"], json!(7));
        assert_eq!(event.payload["data"]["attributes"]["1/6/0"], json!(true));
        assert_eq!(event.payload["data"]["interview_version"], json!(1));
        assert!(events.try_recv().is_err(), "exactly one event");

        // And it is no longer a first read, so ordinary polling takes over.
        assert!(!context.nodes.awaiting_first_interview(7));
        assert_eq!(
            context.nodes.get(7).unwrap().date_commissioned,
            "2023-11-14T22:13:20.000Z",
            "the commissioning date is history, not something an interview sets"
        );
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
