//! Consuming the reports a subscription produces.
//!
//! This is the other end of [`crate::matter::interaction::subscribe`]: the
//! device pushes a `ReportData` whenever a subscribed attribute changes, on an
//! exchange it opens itself, and rs-matter's Interaction Model routes it here
//! with the `(fabric, peer, subscription id)` it belongs to.
//!
//! That identity is the reason this runs inside the Interaction Model rather
//! than off a raw accepted exchange: an exchange on its own does not say which
//! node opened it, and a report that cannot be attributed to a node is worse
//! than no report at all.
//!
//! What comes out the other side is the events clients already know —
//! `attribute_updated`, `endpoint_added`, `endpoint_removed`, `node_updated` —
//! produced by the same code the poller uses. Only the trigger differs: a
//! device saying "this changed" instead of this server asking "what changed?".
//!
//! Matter *events* arrive the same way and have no polled equivalent at all: a
//! button press, a lock operation, a node booting. They become `node_event`,
//! which until now had a shape and a history and nothing to put in them.

use std::sync::Arc;

use rs_matter::dm::{ReportContext, ReportDataHandler};
use rs_matter::im::{EventDataTimestamp, EventResp, IMStatusCode, ReportDataResp};

use crate::api::ServerContext;
use crate::protocol::model::{AttributesData, MatterNodeEvent};
use crate::storage::nodes::Coverage;

use super::interaction::collect_attributes;
use super::subscriptions::Registry;
use super::tlv_json;

/// `timestamp` counts milliseconds since the device booted.
const TIMESTAMP_SYSTEM: u8 = 0;
/// `timestamp` counts milliseconds since the Unix epoch.
const TIMESTAMP_EPOCH: u8 = 1;

/// Turns incoming reports into the protocol's events.
pub struct ReportReceiver {
    context: Arc<ServerContext>,
}

impl ReportReceiver {
    pub fn new(context: Arc<ServerContext>) -> Self {
        Self { context }
    }
}

/// Whether a report is one this controller asked for.
///
/// Its own function because it is the security-relevant decision in this
/// module: accepting a report means writing its values onto a node, and the
/// only thing standing between "node 1 changed" and "node 2's attributes were
/// overwritten" is this check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Apply it: this server established this subscription with this node.
    Accept,
    /// Disown it, so the device tears the subscription down.
    Disown,
}

pub fn verdict(subscriptions: &Registry, node_id: u64, subscription_id: Option<u32>) -> Verdict {
    // A report with no subscription id is an unsolicited one — legal on the
    // wire, but not something this controller ever asked for.
    let Some(subscription_id) = subscription_id else {
        return Verdict::Disown;
    };
    if subscriptions.accepts(node_id, subscription_id) {
        Verdict::Accept
    } else {
        Verdict::Disown
    }
}

impl ReportDataHandler for ReportReceiver {
    async fn handle_report(
        &self,
        ctx: impl ReportContext,
        report: &ReportDataResp<'_>,
    ) -> Result<(), IMStatusCode> {
        let subscription = ctx.subscription();
        let node_id = subscription.peer_node_id;

        if verdict(
            &self.context.subscriptions,
            node_id,
            subscription.subscription_id,
        ) == Verdict::Disown
        {
            log::debug!(
                "Node {} reported on subscription {:?}, which this server does not hold",
                node_id,
                subscription.subscription_id
            );
            return Err(IMStatusCode::InvalidSubscription);
        }

        // The report proves the node is alive whatever it contains, so the
        // liveness note comes before anything that might decline to publish.
        self.context.subscriptions.note_report(node_id);

        let mut attributes = AttributesData::new();
        if let Err(error) = collect_attributes(&mut attributes, report, &mut None) {
            log::warn!("Node {}'s report could not be decoded: {}", node_id, error);
            return Err(IMStatusCode::Failure);
        }

        if !attributes.is_empty() {
            // A report carries only what changed, never the whole node.
            crate::monitor::publish_changes(&self.context, node_id, attributes, Coverage::Partial);
        }

        for event in node_events(node_id, report) {
            self.context
                .events
                .publish(crate::protocol::events::Event::node_event(&event));
            self.context.record_node_event(event);
        }

        Ok(())
    }
}

/// Turn a report's event entries into the protocol's `node_event` payloads.
///
/// A per-event decode failure drops that event and keeps the rest: one
/// unreadable event is not a reason to lose a lock's audit trail.
fn node_events(node_id: u64, report: &ReportDataResp<'_>) -> Vec<MatterNodeEvent> {
    let Some(reports) = report.event_reports.as_ref() else {
        return Vec::new();
    };

    let mut events = Vec::new();
    // A device may delta-encode a timestamp against the previous event of the
    // same kind, so the previous one has to be remembered while walking the
    // list.
    let mut last_epoch: Option<u64> = None;
    let mut last_system: Option<u64> = None;

    for entry in reports.iter() {
        let Ok(EventResp::Data(data)) = entry else {
            // A status entry means the node declined one event path — normal
            // on a wildcard subscription, and nothing to report.
            continue;
        };

        let (timestamp, timestamp_type) = match data.timestamp {
            EventDataTimestamp::EpochTimestamp(value) => {
                last_epoch = Some(value);
                (value, TIMESTAMP_EPOCH)
            }
            EventDataTimestamp::SystemTimestamp(value) => {
                last_system = Some(value);
                (value, TIMESTAMP_SYSTEM)
            }
            // A delta with nothing to add to is reported as it arrived; the
            // alternative is inventing a base for it.
            EventDataTimestamp::DeltaEpochTimestamp(delta) => {
                let value = last_epoch.map_or(delta, |base| base.saturating_add(delta));
                last_epoch = Some(value);
                (value, TIMESTAMP_EPOCH)
            }
            EventDataTimestamp::DeltaSystemTimestamp(delta) => {
                let value = last_system.map_or(delta, |base| base.saturating_add(delta));
                last_system = Some(value);
                (value, TIMESTAMP_SYSTEM)
            }
        };

        let value = match tlv_json::to_json(&data.data) {
            Ok(value) => value,
            Err(error) => {
                log::warn!(
                    "Node {} sent an event this server could not decode: {:?}",
                    node_id,
                    error.code()
                );
                continue;
            }
        };

        events.push(MatterNodeEvent {
            node_id,
            endpoint_id: data.path.endpoint.unwrap_or(0),
            cluster_id: data.path.cluster.unwrap_or(0),
            event_id: data.path.event.unwrap_or(0),
            event_number: data.event_number,
            priority: data.priority as u8,
            timestamp,
            timestamp_type,
            data: value,
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use rs_matter::im::{
        EventData, EventPath, EventPriority, EventStatus, IMStatusCode, ReportDataRespTag,
    };
    use rs_matter::tlv::{FromTLV, TLVElement, TLVTag, TLVWrite, ToTLV};
    use rs_matter::utils::storage::WriteBuf;
    use serde_json::json;

    const MINUTE: Duration = Duration::from_secs(60);

    fn path(endpoint: u16, cluster: u32, event: u32) -> EventPath {
        EventPath::from_gp(&rs_matter::im::GenericPath::new(
            Some(endpoint),
            Some(cluster),
            Some(event),
        ))
    }

    /// One event's payload: a struct with a single field, which is what a
    /// Switch cluster's `InitialPress` looks like.
    fn payload(buf: &mut [u8]) -> usize {
        let mut wb = WriteBuf::new(buf);
        wb.start_struct(&TLVTag::Anonymous).unwrap();
        wb.u8(&TLVTag::Context(0), 1).unwrap();
        wb.end_container().unwrap();
        wb.get_tail()
    }

    /// Encode a report carrying the given event entries, then parse it back —
    /// so the test exercises the same decode path a device's bytes take.
    fn report(entries: &[EventResp<'_>], buf: &mut [u8]) -> usize {
        let mut wb = WriteBuf::new(buf);
        wb.start_struct(&TLVTag::Anonymous).unwrap();
        wb.start_array(&TLVTag::Context(ReportDataRespTag::EventReports as u8))
            .unwrap();
        for entry in entries {
            entry.to_tlv(&TLVTag::Anonymous, &mut wb).unwrap();
        }
        wb.end_container().unwrap();
        wb.end_container().unwrap();
        wb.get_tail()
    }

    fn decode(bytes: &[u8], node_id: u64) -> Vec<MatterNodeEvent> {
        let element = TLVElement::new(bytes);
        let parsed = ReportDataResp::from_tlv(&element).expect("a readable report");
        node_events(node_id, &parsed)
    }

    #[test]
    fn an_event_report_becomes_a_node_event() {
        let mut data = [0u8; 32];
        let len = payload(&mut data);
        let entry = EventResp::Data(EventData::new(
            path(1, 59, 1),
            7,
            EventPriority::Info,
            EventDataTimestamp::EpochTimestamp(1_700_000_000_000),
            TLVElement::new(&data[..len]),
        ));

        let mut buf = [0u8; 256];
        let len = report(&[entry], &mut buf);
        let events = decode(&buf[..len], 42);

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.node_id, 42);
        assert_eq!(event.endpoint_id, 1);
        assert_eq!(event.cluster_id, 59);
        assert_eq!(event.event_id, 1);
        assert_eq!(event.event_number, 7);
        assert_eq!(event.priority, EventPriority::Info as u8);
        assert_eq!(event.timestamp, 1_700_000_000_000);
        assert_eq!(event.timestamp_type, TIMESTAMP_EPOCH);
        assert_eq!(event.data, json!({ "0": 1 }));
    }

    #[test]
    fn a_system_timestamp_is_told_apart_from_an_epoch_one() {
        let mut data = [0u8; 32];
        let len = payload(&mut data);
        let entry = EventResp::Data(EventData::new(
            path(0, 40, 0),
            1,
            EventPriority::Critical,
            EventDataTimestamp::SystemTimestamp(5_000),
            TLVElement::new(&data[..len]),
        ));

        let mut buf = [0u8; 256];
        let len = report(&[entry], &mut buf);
        let events = decode(&buf[..len], 1);
        assert_eq!(events[0].timestamp, 5_000);
        assert_eq!(events[0].timestamp_type, TIMESTAMP_SYSTEM);
    }

    /// A device may encode each event's timestamp as a delta on the previous
    /// one of the same kind, so the previous one has to be carried along.
    #[test]
    fn delta_timestamps_accumulate_on_the_one_before() {
        let mut data = [0u8; 32];
        let len = payload(&mut data);
        let base = EventResp::Data(EventData::new(
            path(1, 59, 1),
            1,
            EventPriority::Info,
            EventDataTimestamp::EpochTimestamp(1_000),
            TLVElement::new(&data[..len]),
        ));
        let delta = EventResp::Data(EventData::new(
            path(1, 59, 2),
            2,
            EventPriority::Info,
            EventDataTimestamp::DeltaEpochTimestamp(250),
            TLVElement::new(&data[..len]),
        ));

        let mut buf = [0u8; 512];
        let len = report(&[base, delta], &mut buf);
        let events = decode(&buf[..len], 1);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].timestamp, 1_000);
        assert_eq!(events[1].timestamp, 1_250);
        assert_eq!(events[1].timestamp_type, TIMESTAMP_EPOCH);
    }

    /// A wildcard subscription routinely draws status entries for paths the
    /// device does not have. They are not events.
    #[test]
    fn status_entries_are_not_reported_as_events() {
        let entry = EventResp::Status(EventStatus::new(
            path(1, 59, 1),
            IMStatusCode::UnsupportedEvent,
            None,
        ));
        let mut buf = [0u8; 256];
        let len = report(&[entry], &mut buf);
        assert!(decode(&buf[..len], 1).is_empty());
    }

    #[test]
    fn a_report_with_no_events_produces_none() {
        let mut buf = [0u8; 64];
        let len = report(&[], &mut buf);
        assert!(decode(&buf[..len], 1).is_empty());
    }

    #[test]
    fn a_report_from_a_subscription_this_server_holds_is_accepted() {
        let subscriptions = Registry::new();
        subscriptions.established(1, 42, MINUTE);
        assert_eq!(verdict(&subscriptions, 1, Some(42)), Verdict::Accept);
    }

    /// The case this check exists for: subscription ids are only unique per
    /// device, so two nodes may hold the same one. Accepting on the id alone
    /// would write one node's attributes onto another.
    #[test]
    fn the_same_id_from_a_different_node_is_disowned() {
        let subscriptions = Registry::new();
        subscriptions.established(1, 42, MINUTE);
        assert_eq!(verdict(&subscriptions, 2, Some(42)), Verdict::Disown);
    }

    #[test]
    fn a_report_for_a_subscription_this_server_never_made_is_disowned() {
        let subscriptions = Registry::new();
        // What a device left over from a previous run of this server — or from
        // the matterjs-server installation whose fabric was imported — sends.
        assert_eq!(verdict(&subscriptions, 1, Some(7)), Verdict::Disown);
    }

    #[test]
    fn an_unsolicited_report_is_disowned() {
        let subscriptions = Registry::new();
        subscriptions.established(1, 42, MINUTE);
        assert_eq!(verdict(&subscriptions, 1, None), Verdict::Disown);
    }
}
