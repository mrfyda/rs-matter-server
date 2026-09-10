//! What this controller is subscribed to.
//!
//! A subscription lives on the device, not here: the controller asks for one,
//! the device issues an id and then pushes `ReportData` on exchanges of its
//! own for as long as it holds it. Two parties on this side need to agree
//! about that — the actor, which establishes subscriptions, and the report
//! handler, which receives their reports — so what they agree about lives
//! here.
//!
//! **A report is only accepted for a subscription this server established.**
//! The id alone is not an identity: the Matter spec only requires it to be
//! unique per publisher, so two devices may well both pick 1. It is the
//! `(node, id)` pair that identifies a subscription, and rs-matter supplies
//! the node with every report. A report that matches nothing is disowned with
//! `InvalidSubscription`, which is how a device learns to stop reporting —
//! the case that matters being a device that still holds a subscription from
//! a previous run of this server, or from the matterjs-server installation
//! whose fabric was imported.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A subscription this controller holds on one node.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The id the device issued.
    subscription_id: u32,
    /// The longest the device may stay silent before it owes a report.
    max_interval: Duration,
    /// When the last report arrived; the priming report counts.
    last_report: Instant,
}

/// The subscriptions this controller holds, keyed by node.
///
/// One per node: a wildcard subscription already covers everything a node has,
/// so a second would be duplicate traffic.
#[derive(Default)]
pub struct Registry {
    entries: Mutex<BTreeMap<u64, Entry>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a subscription the device has just confirmed.
    pub fn established(&self, node_id: u64, subscription_id: u32, max_interval: Duration) {
        self.entries.lock().unwrap().insert(
            node_id,
            Entry {
                subscription_id,
                max_interval,
                last_report: Instant::now(),
            },
        );
    }

    /// Whether a report carrying this id belongs to a subscription this server
    /// established with this node.
    pub fn accepts(&self, node_id: u64, subscription_id: u32) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(&node_id)
            .is_some_and(|entry| entry.subscription_id == subscription_id)
    }

    /// Note that a node reported, which is also the liveness signal.
    pub fn note_report(&self, node_id: u64) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(&node_id) {
            entry.last_report = Instant::now();
        }
    }

    /// Forget a node's subscription — it was torn down, or the node is gone.
    pub fn forget(&self, node_id: u64) {
        self.entries.lock().unwrap().remove(&node_id);
    }

    /// Whether a node is subscribed and still reporting.
    ///
    /// A device owes a report every `max_interval` even when nothing changed,
    /// so silence for twice that long means the subscription is gone — the
    /// device rebooted, or dropped it to make room. Twice, rather than once,
    /// because a report is due *at* the interval and a slow network is not a
    /// dead subscription.
    pub fn is_live(&self, node_id: u64) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(&node_id)
            .is_some_and(|entry| entry.last_report.elapsed() < entry.max_interval * 2)
    }

    /// Drop every subscription that has stopped reporting, and say which.
    pub fn forget_silent(&self) -> Vec<u64> {
        let mut entries = self.entries.lock().unwrap();
        let silent: Vec<u64> = entries
            .iter()
            .filter(|(_, entry)| entry.last_report.elapsed() >= entry.max_interval * 2)
            .map(|(node_id, _)| *node_id)
            .collect();
        for node_id in &silent {
            entries.remove(node_id);
        }
        silent
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn a_report_is_accepted_only_for_the_node_it_was_established_with() {
        let registry = Registry::new();
        registry.established(1, 7, MINUTE);

        assert!(registry.accepts(1, 7));
        // The same id from a different node is a different subscription, and
        // ids are only unique per device — this is the collision that makes
        // the node id part of the key.
        assert!(!registry.accepts(2, 7));
        assert!(!registry.accepts(1, 8));
    }

    #[test]
    fn resubscribing_replaces_the_previous_id() {
        let registry = Registry::new();
        registry.established(1, 7, MINUTE);
        registry.established(1, 9, MINUTE);
        assert!(!registry.accepts(1, 7));
        assert!(registry.accepts(1, 9));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn forgetting_disowns_further_reports() {
        let registry = Registry::new();
        registry.established(1, 7, MINUTE);
        registry.forget(1);
        assert!(!registry.accepts(1, 7));
        assert!(registry.is_empty());
    }

    #[test]
    fn a_node_that_has_stopped_reporting_is_not_live() {
        let registry = Registry::new();
        // Already overdue: the interval is zero, so any elapsed time is twice
        // it.
        registry.established(1, 7, Duration::ZERO);
        assert!(!registry.is_live(1));
        assert_eq!(registry.forget_silent(), vec![1]);
        assert!(registry.is_empty());

        registry.established(2, 8, MINUTE);
        assert!(registry.is_live(2));
        assert!(registry.forget_silent().is_empty());
    }

    #[test]
    fn a_node_with_no_subscription_is_not_live() {
        let registry = Registry::new();
        assert!(!registry.is_live(1));
        assert!(!registry.accepts(1, 0));
    }
}
