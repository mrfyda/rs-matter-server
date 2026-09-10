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

use std::sync::Arc;

use rs_matter::dm::{ReportContext, ReportDataHandler};
use rs_matter::im::{IMStatusCode, ReportDataResp};

use crate::api::ServerContext;
use crate::protocol::model::AttributesData;

use super::interaction::collect_attributes;
use super::subscriptions::Registry;

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

pub fn verdict(
    subscriptions: &Registry,
    node_id: u64,
    subscription_id: Option<u32>,
) -> Verdict {
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
            crate::monitor::publish_changes(&self.context, node_id, attributes);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MINUTE: Duration = Duration::from_secs(60);

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
