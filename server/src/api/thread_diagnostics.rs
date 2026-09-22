//! Thread diagnostics: what is known per network, and how a collection runs.
//!
//! A collection is slow — the border router queries the mesh one node at a
//! time, and the reference gives a mesh about twenty seconds to answer — so it
//! runs on a thread of its own and publishes as it goes: every batch it
//! produces is stored here and announced as `thread_diagnostics_updated`,
//! first with `in_progress` as nodes arrive and finally complete, or with the
//! reason it is not. A caller that needs an answer now waits a few seconds and
//! takes whatever the collection has by then, which is what the reference
//! does too.
//!
//! The source is a border router's OpenThread REST API, found by probing the
//! addresses mDNS advertised for the network on port 8081. MeshCoP, which the
//! reference prefers, is not spoken here — see the roadmap — so a network
//! whose border routers serve no REST API answers `no_credentials`, as the
//! reference does when it has neither.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::matter::mdns_browser::ServiceInstance;
use crate::matter::otbr_rest::{self, Capability, Client};
use crate::protocol::events::Event;
use crate::protocol::model::{
    ThreadDiagnosticsBatch, ThreadDiagnosticsNode, ThreadDiagnosticsPartial,
    ThreadDiagnosticsSource,
};

use super::EventBus;

/// How long a complete batch is reused. The reference's own hour: a mesh does
/// not change shape often, and collecting is slow enough to matter.
const TTL: Duration = Duration::from_secs(3600);
/// How long a network whose border routers answered no REST probe is left
/// alone before it is probed again. Border routers do not grow a REST API
/// between one minute and the next, and each probe is a round of timeouts.
const PROBE_MISS_TTL: Duration = Duration::from_secs(600);
/// How long the synchronous form waits for a collection before answering with
/// what it has. The reference's five seconds.
pub const FIRST_BATCH_WAIT: Duration = Duration::from_secs(5);
/// How often a running collection announces its progress at most. Nodes tend
/// to arrive in a burst as the border router works through them, and one
/// event per node would be one full batch per node.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// A Thread network as discovery names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadNetwork {
    /// 16 uppercase hex characters.
    pub ext_pan_id_hex: String,
    pub name: String,
    /// The border routers that advertised it, as mDNS answered.
    pub border_routers: Vec<ServiceInstance>,
}

/// What probing a network's border routers found, and when.
struct Probe {
    capability: Option<Capability>,
    probed_at: Instant,
}

#[derive(Default)]
struct Inner {
    /// Keyed by uppercase extended PAN id.
    batches: Mutex<BTreeMap<String, ThreadDiagnosticsBatch>>,
    capabilities: Mutex<BTreeMap<String, Probe>>,
    in_flight: Mutex<BTreeMap<String, Arc<Collection>>>,
    /// Where border routers serve REST; `otbr_rest::DEFAULT_PORT` outside tests.
    rest_port: u16,
}

/// Thread diagnostics, kept per network for as long as they are worth reusing,
/// plus what is known about how to collect them.
///
/// **Only a complete batch is a cache hit.** A partial one records why it is
/// partial, and every reason is something that can stop being true — a border
/// router coming back, a collection still running — so holding one for the
/// full hour would keep answering with a failure that has since been fixed.
///
/// Cheap to clone: a collection thread carries one.
#[derive(Clone)]
pub struct ThreadDiagnosticsCache {
    inner: Arc<Inner>,
}

impl Default for ThreadDiagnosticsCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadDiagnosticsCache {
    pub fn new() -> Self {
        Self::with_rest_port(otbr_rest::DEFAULT_PORT)
    }

    pub fn with_rest_port(rest_port: u16) -> Self {
        Self {
            inner: Arc::new(Inner {
                rest_port,
                ..Inner::default()
            }),
        }
    }

    pub fn store(&self, batch: ThreadDiagnosticsBatch) {
        self.inner
            .batches
            .lock()
            .unwrap()
            .insert(batch.ext_pan_id_hex.clone(), batch);
    }

    /// A batch worth answering with without collecting again.
    pub fn fresh(&self, ext_pan_id: &str) -> Option<ThreadDiagnosticsBatch> {
        let batches = self.inner.batches.lock().unwrap();
        let batch = batches.get(ext_pan_id)?;
        if batch.partial_reason.is_some() {
            return None;
        }
        let age = chrono::Utc::now().timestamp_millis() - batch.collected_at;
        (age >= 0 && (age as u128) < TTL.as_millis()).then(|| batch.clone())
    }

    /// Everything held, fresh or not: what the no-argument form answers with
    /// before a refresh has produced anything newer.
    pub fn all(&self) -> Vec<ThreadDiagnosticsBatch> {
        self.inner
            .batches
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    /// The collection running for a network, or a new one.
    ///
    /// Two callers asking about the same network share one collection rather
    /// than racing two sweeps against the same border router, whose task
    /// queue is global. `force` bears on the caches, not on a run already in
    /// progress: it is joined like any other.
    pub fn collection(
        &self,
        network: &ThreadNetwork,
        events: Arc<EventBus>,
        force: bool,
    ) -> Arc<Collection> {
        let mut in_flight = self.inner.in_flight.lock().unwrap();
        if let Some(existing) = in_flight.get(&network.ext_pan_id_hex) {
            return existing.clone();
        }
        let (done_tx, done_rx) = async_channel::bounded::<()>(1);
        let collection = Arc::new(Collection {
            state: Mutex::new(CollectionState {
                batch: ThreadDiagnosticsBatch {
                    ext_pan_id_hex: network.ext_pan_id_hex.clone(),
                    network_name: network.name.clone(),
                    collected_at: chrono::Utc::now().timestamp_millis(),
                    source: ThreadDiagnosticsSource::None,
                    nodes: Vec::new(),
                    partial_reason: Some(ThreadDiagnosticsPartial::InProgress),
                },
                finished: false,
            }),
            done: done_rx,
        });
        in_flight.insert(network.ext_pan_id_hex.clone(), collection.clone());
        drop(in_flight);

        let run = Run {
            cache: self.clone(),
            events,
            network: network.clone(),
            force,
            collection: collection.clone(),
        };
        thread::Builder::new()
            .name(format!("thread-diag-{}", network.ext_pan_id_hex))
            .spawn(move || {
                run.execute();
                // Waiters wake when the sender goes; the run is out of the
                // in-flight map first, so the next asker starts afresh.
                drop(done_tx);
            })
            .expect("spawning a collection thread");
        collection
    }

    /// The REST capability of a network's border routers: remembered from the
    /// last probe, or probed now. A miss is remembered too, for a while, so a
    /// network with no REST in it does not cost a round of timeouts per ask.
    fn rest_capability(&self, network: &ThreadNetwork, force: bool) -> Option<Capability> {
        if !force {
            let capabilities = self.inner.capabilities.lock().unwrap();
            if let Some(probe) = capabilities.get(&network.ext_pan_id_hex) {
                match &probe.capability {
                    Some(capability) => return Some(capability.clone()),
                    None if probe.probed_at.elapsed() < PROBE_MISS_TTL => return None,
                    None => {}
                }
            }
        }
        let capability = probe_border_routers(network, self.inner.rest_port);
        self.inner.capabilities.lock().unwrap().insert(
            network.ext_pan_id_hex.clone(),
            Probe {
                capability: capability.clone(),
                probed_at: Instant::now(),
            },
        );
        capability
    }

    /// Drop what was known about a network's border routers, because one of
    /// them stopped answering: the next collection probes again.
    fn forget_capability(&self, ext_pan_id: &str) {
        self.inner.capabilities.lock().unwrap().remove(ext_pan_id);
    }

    fn finish_in_flight(&self, ext_pan_id: &str) {
        self.inner.in_flight.lock().unwrap().remove(ext_pan_id);
    }
}

/// Try every border router on a network, on every address it advertised,
/// until one answers as a REST endpoint that can collect diagnostics.
///
/// Link-local addresses are skipped: reaching one needs the interface it is
/// local to, which the mDNS answer does not carry. A border router with any
/// REST API also advertises a routable address.
fn probe_border_routers(network: &ThreadNetwork, port: u16) -> Option<Capability> {
    let mut fallback = None;
    for router in &network.border_routers {
        for address in &router.addresses {
            if is_link_local(address) {
                continue;
            }
            match Client::new(*address, port).probe() {
                Ok(capability) if capability.diagnostics_api != otbr_rest::DiagnosticsApi::None => {
                    if capability.ext_pan_id_hex != network.ext_pan_id_hex {
                        log::debug!(
                            "{} says it is on network {}, but mDNS said {}",
                            capability.base_url,
                            capability.ext_pan_id_hex,
                            network.ext_pan_id_hex
                        );
                    }
                    log::info!(
                        "Thread network {} has a REST border router at {} ({:?} API)",
                        network.ext_pan_id_hex,
                        capability.base_url,
                        capability.diagnostics_api
                    );
                    return Some(capability);
                }
                Ok(capability) => {
                    log::debug!(
                        "{} serves REST but no diagnostics; looking further",
                        capability.base_url
                    );
                    fallback.get_or_insert(capability);
                }
                Err(error) => log::debug!("No REST border router at {}: {}", address, error),
            }
        }
    }
    // A border router with REST but no diagnostics API is still worth
    // recording: the collection then says `rest_protocol`, which is truer than
    // `no_credentials`.
    fallback
}

fn is_link_local(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_link_local(),
        IpAddr::V6(address) => (address.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// One collection in flight for one network.
pub struct Collection {
    state: Mutex<CollectionState>,
    /// Closed when the collection finishes, which is what wakes a waiter.
    done: async_channel::Receiver<()>,
}

struct CollectionState {
    batch: ThreadDiagnosticsBatch,
    finished: bool,
}

impl Collection {
    /// The batch as it stands. Mid-collection it says `in_progress`, or
    /// `rest_no_responses_yet` while nothing has answered; finished, it is
    /// the final batch.
    pub fn snapshot(&self) -> ThreadDiagnosticsBatch {
        let state = self.state.lock().unwrap();
        let mut batch = state.batch.clone();
        if !state.finished {
            batch.collected_at = chrono::Utc::now().timestamp_millis();
            batch.partial_reason = Some(if batch.nodes.is_empty() {
                match batch.source {
                    ThreadDiagnosticsSource::OtbrRest => {
                        ThreadDiagnosticsPartial::RestNoResponsesYet
                    }
                    _ => ThreadDiagnosticsPartial::InProgress,
                }
            } else {
                ThreadDiagnosticsPartial::InProgress
            });
        }
        batch
    }

    pub fn is_finished(&self) -> bool {
        self.state.lock().unwrap().finished
    }

    /// Wait up to `timeout` for the collection to finish, then answer with
    /// whatever it has.
    pub async fn first_batch(&self, timeout: Duration) -> ThreadDiagnosticsBatch {
        let done = self.done.clone();
        futures_lite::future::or(
            async {
                // Closed means finished; a message would too, but none is sent.
                let _ = done.recv().await;
            },
            async {
                async_io::Timer::after(timeout).await;
            },
        )
        .await;
        self.snapshot()
    }
}

/// The collection thread's work.
struct Run {
    cache: ThreadDiagnosticsCache,
    events: Arc<EventBus>,
    network: ThreadNetwork,
    force: bool,
    collection: Arc<Collection>,
}

impl Run {
    fn execute(self) {
        let started = Instant::now();
        let Some(capability) = self.cache.rest_capability(&self.network, self.force) else {
            log::info!(
                "Thread network {} has no border router serving diagnostics over REST",
                self.network.ext_pan_id_hex
            );
            self.finish(
                ThreadDiagnosticsSource::None,
                Some(ThreadDiagnosticsPartial::NoCredentials),
            );
            return;
        };
        self.collection.state.lock().unwrap().batch.source = ThreadDiagnosticsSource::OtbrRest;

        let client = Client::for_base_url(&capability.base_url);
        let last_published = Mutex::new(Instant::now());
        let result = otbr_rest::collect(&client, &capability, &|node| {
            self.add_node(node);
            let mut last = last_published.lock().unwrap();
            if last.elapsed() >= PROGRESS_INTERVAL {
                *last = Instant::now();
                self.publish(self.collection.snapshot());
            }
        });

        match result {
            Ok(()) => {
                let count = self.collection.state.lock().unwrap().batch.nodes.len();
                log::info!(
                    "Thread network {}: {} node(s) collected over REST in {:?}",
                    self.network.ext_pan_id_hex,
                    count,
                    started.elapsed()
                );
                self.finish(ThreadDiagnosticsSource::OtbrRest, None);
            }
            Err(error) => {
                log::warn!(
                    "Thread network {}: collection over REST failed after {:?}: {}",
                    self.network.ext_pan_id_hex,
                    started.elapsed(),
                    error
                );
                if matches!(error, otbr_rest::RestError::Unreachable(_)) {
                    self.cache.forget_capability(&self.network.ext_pan_id_hex);
                }
                self.finish(
                    ThreadDiagnosticsSource::OtbrRest,
                    Some(error.partial_reason()),
                );
            }
        }
    }

    /// Record a node, replacing an earlier view of the same one.
    fn add_node(&self, node: ThreadDiagnosticsNode) {
        let mut state = self.collection.state.lock().unwrap();
        let same = |existing: &ThreadDiagnosticsNode| {
            (node.ext_mac_address.is_some() && existing.ext_mac_address == node.ext_mac_address)
                || (node.ext_mac_address.is_none()
                    && node.rloc16.is_some()
                    && existing.rloc16 == node.rloc16)
        };
        match state.batch.nodes.iter_mut().find(|existing| same(existing)) {
            Some(existing) => *existing = node,
            None => state.batch.nodes.push(node),
        }
    }

    fn finish(&self, source: ThreadDiagnosticsSource, reason: Option<ThreadDiagnosticsPartial>) {
        let batch = {
            let mut state = self.collection.state.lock().unwrap();
            state.batch.source = source;
            state.batch.partial_reason = reason;
            state.batch.collected_at = chrono::Utc::now().timestamp_millis();
            state.finished = true;
            state.batch.clone()
        };
        // Out of the in-flight map before anyone is woken, so a caller that
        // asks again the moment it has this answer starts a new collection
        // rather than joining a finished one.
        self.cache.finish_in_flight(&self.network.ext_pan_id_hex);
        self.publish(batch);
    }

    /// Store a batch and announce it.
    fn publish(&self, batch: ThreadDiagnosticsBatch) {
        self.cache.store(batch.clone());
        self.events.publish(Event::thread_diagnostics_updated(
            serde_json::to_value(&batch).unwrap_or(Value::Null),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matter::otbr_rest::mock::MockBorderRouter;
    use futures_lite::future::block_on;
    use serde_json::json;
    use std::collections::BTreeMap as Map;

    fn network(routers: &[&str]) -> ThreadNetwork {
        ThreadNetwork {
            ext_pan_id_hex: "1122334455667788".into(),
            name: "MyThreadNet".into(),
            border_routers: routers
                .iter()
                .map(|address| ServiceInstance {
                    instance_name: "br".into(),
                    host_name: Some("br.local".into()),
                    port: Some(49191),
                    addresses: vec![address.parse().unwrap()],
                    txt: Map::new(),
                })
                .collect(),
        }
    }

    fn node_json() -> String {
        json!({ "networkName": "MyThreadNet", "extPanId": "1122334455667788" }).to_string()
    }

    /// A border router serving the legacy API: one read, two nodes.
    fn legacy_router() -> MockBorderRouter {
        MockBorderRouter::start(|request| match request.path.as_str() {
            "/node" => (200, node_json()),
            "/diagnostics" => (
                200,
                json!([
                    { "ExtAddress": "96518e5497d5b9f3", "Rloc16": 20480 },
                    { "ExtAddress": "2a55d952bc7b4008", "Rloc16": 20484 }
                ])
                .to_string(),
            ),
            _ => (404, String::new()),
        })
    }

    fn drain(events: &async_channel::Receiver<Event>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(event) = events.try_recv() {
            out.push(event.payload["data"].clone());
        }
        out
    }

    #[test]
    fn a_network_with_no_border_router_answers_no_credentials_at_once() {
        let cache = ThreadDiagnosticsCache::new();
        let events = Arc::new(EventBus::new());
        let received = events.subscribe();
        let collection = cache.collection(&network(&[]), events, false);

        let batch = block_on(collection.first_batch(Duration::from_secs(5)));
        assert!(collection.is_finished());
        assert_eq!(batch.source, ThreadDiagnosticsSource::None);
        assert_eq!(
            batch.partial_reason,
            Some(ThreadDiagnosticsPartial::NoCredentials)
        );
        assert!(batch.nodes.is_empty());
        // Announced, and held — but not as a hit.
        let published = drain(&received);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0]["partialReason"], json!("no_credentials"));
        assert_eq!(cache.all().len(), 1);
        assert!(cache.fresh("1122334455667788").is_none());
    }

    #[test]
    fn a_rest_border_router_produces_a_complete_batch() {
        let router = legacy_router();
        let cache = ThreadDiagnosticsCache::with_rest_port(router.port);
        let events = Arc::new(EventBus::new());
        let received = events.subscribe();
        let collection = cache.collection(&network(&["127.0.0.1"]), events, false);

        let batch = block_on(collection.first_batch(Duration::from_secs(10)));
        assert!(collection.is_finished(), "{:?}", batch);
        assert_eq!(batch.source, ThreadDiagnosticsSource::OtbrRest);
        assert_eq!(batch.partial_reason, None);
        assert_eq!(batch.nodes.len(), 2);
        assert_eq!(batch.ext_pan_id_hex, "1122334455667788");
        assert_eq!(batch.network_name, "MyThreadNet");

        // The final batch was published, and it is now a cache hit.
        let published = drain(&received);
        let last = published.last().unwrap();
        assert_eq!(last["source"], json!("otbr-rest"));
        assert!(last.get("partialReason").is_none());
        assert_eq!(last["nodes"].as_array().unwrap().len(), 2);
        assert!(cache.fresh("1122334455667788").is_some());

        // The probe is remembered: a second collection asks /node no more.
        let probes_before = router
            .requests()
            .iter()
            .filter(|r| r.path == "/node")
            .count();
        let again = cache.collection(&network(&["127.0.0.1"]), Arc::new(EventBus::new()), false);
        block_on(again.first_batch(Duration::from_secs(10)));
        let probes_after = router
            .requests()
            .iter()
            .filter(|r| r.path == "/node")
            .count();
        assert_eq!(probes_before, probes_after);
    }

    #[test]
    fn a_border_router_that_vanishes_is_reported_and_forgotten() {
        let router = legacy_router();
        let port = router.port;
        let cache = ThreadDiagnosticsCache::with_rest_port(port);
        // Probe it while it is up...
        assert!(cache
            .rest_capability(&network(&["127.0.0.1"]), false)
            .is_some());
        drop(router);
        // ...then collect once it is gone.
        let collection =
            cache.collection(&network(&["127.0.0.1"]), Arc::new(EventBus::new()), false);
        let batch = block_on(collection.first_batch(Duration::from_secs(10)));
        assert!(collection.is_finished());
        assert_eq!(batch.source, ThreadDiagnosticsSource::OtbrRest);
        assert_eq!(
            batch.partial_reason,
            Some(ThreadDiagnosticsPartial::RestUnreachable)
        );
        // The capability went with it, so the next run probes again rather
        // than failing the same way for an hour.
        assert!(cache
            .inner
            .capabilities
            .lock()
            .unwrap()
            .get("1122334455667788")
            .is_none());
    }

    #[test]
    fn a_probe_miss_is_remembered_and_force_probes_again() {
        let nothing = MockBorderRouter::start(|_| (404, String::new()));
        let cache = ThreadDiagnosticsCache::with_rest_port(nothing.port);
        let network = network(&["127.0.0.1"]);
        assert!(cache.rest_capability(&network, false).is_none());
        assert!(cache.rest_capability(&network, false).is_none());
        assert_eq!(nothing.requests().len(), 1);
        assert!(cache.rest_capability(&network, true).is_none());
        assert_eq!(nothing.requests().len(), 2);
    }

    #[test]
    fn link_local_addresses_are_not_probed() {
        let cache = ThreadDiagnosticsCache::with_rest_port(9);
        // Neither of these is dialled, so this returns without a timeout.
        let started = Instant::now();
        assert!(cache
            .rest_capability(&network(&["fe80::1", "169.254.1.1"]), false)
            .is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn concurrent_askers_share_one_collection() {
        let cache = ThreadDiagnosticsCache::new();
        let events = Arc::new(EventBus::new());
        let first = cache.collection(&network(&[]), events.clone(), false);
        let second = cache.collection(&network(&[]), events, true);
        // The same run, or the first already finished and the second is new;
        // either way there is never a second run alongside a live one.
        let same = Arc::ptr_eq(&first, &second);
        assert!(same || first.is_finished());
    }

    #[test]
    fn an_unfinished_snapshot_says_so() {
        let (_tx, rx) = async_channel::bounded::<()>(1);
        let collection = Collection {
            state: Mutex::new(CollectionState {
                batch: ThreadDiagnosticsBatch {
                    ext_pan_id_hex: "1122334455667788".into(),
                    network_name: "n".into(),
                    collected_at: 0,
                    source: ThreadDiagnosticsSource::OtbrRest,
                    nodes: Vec::new(),
                    partial_reason: None,
                },
                finished: false,
            }),
            done: rx,
        };
        assert_eq!(
            collection.snapshot().partial_reason,
            Some(ThreadDiagnosticsPartial::RestNoResponsesYet)
        );
        collection
            .state
            .lock()
            .unwrap()
            .batch
            .nodes
            .push(ThreadDiagnosticsNode::default());
        assert_eq!(
            collection.snapshot().partial_reason,
            Some(ThreadDiagnosticsPartial::InProgress)
        );
        // A waiter on an unfinished collection gets the snapshot at the
        // deadline, not nothing.
        let batch = block_on(collection.first_batch(Duration::from_millis(50)));
        assert_eq!(batch.nodes.len(), 1);
    }
}
