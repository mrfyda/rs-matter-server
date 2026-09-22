//! The OpenThread Border Router's REST API, as a source of Thread diagnostics.
//!
//! An `ot-br-posix` border router built with its REST server answers on port
//! 8081, and two generations of it exist. The current one models a
//! diagnostic query as a *task*: `POST /api/actions` with a
//! `getNetworkDiagnosticTask` naming one destination, poll
//! `/api/actions/<id>` until it settles, then read the item its result points
//! at under `/api/diagnostics`. The older one served the whole mesh from a
//! single `GET /diagnostics`, with PascalCase keys. Both are spoken here; a
//! probe of `/node` — which every build serves — tells them apart, and the
//! keys are normalised before anything reads them.
//!
//! Plain HTTP over the infrastructure link, and no credentials: which is also
//! the limit. Apple and Google border routers expose no REST API, so a mesh
//! behind one of those cannot be collected this way.
//!
//! Everything here blocks — it is `ureq` — and runs on the collection thread
//! `api::thread_diagnostics` spawns, never on the Matter executor. The names
//! of the types asked for and the keys read back are ot-br-posix's own
//! (`src/rest/names.hpp` and `diagnostic_types.hpp`), not the reference's,
//! where the two disagree.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::protocol::model::{
    ThreadChildTableEntry, ThreadConnectivity, ThreadDiagnosticsNode, ThreadDiagnosticsPartial,
    ThreadLeaderData, ThreadMacCounters, ThreadMleCounters, ThreadMode, ThreadRoute64,
    ThreadRoute64Entry,
};

/// Where `ot-br-posix` serves its REST API unless configured otherwise.
pub const DEFAULT_PORT: u16 = 8081;

/// A probe is one request against every address of every border router on a
/// network, most of which do not serve REST at all, so it is kept short.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a task is asked whether it has settled.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long one node's task may take before it is given up on here. The
/// border router is told to give up on the mesh side sooner, so this is the
/// margin for it to notice and say so.
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const DIAG_TASK_TIMEOUT_S: u32 = 10;
/// The device-directory refresh walks every router, so it is allowed longer.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(20);
const DISCOVER_TASK_TIMEOUT_S: u32 = 15;
/// `updateDeviceCollectionTask` requires all four of its attributes; these are
/// the values ot-br-posix's own examples use, with the count a ceiling.
const DISCOVER_MAX_AGE_S: u32 = 30;
const DISCOVER_MAX_RETRIES: u32 = 5;
const DISCOVER_DEVICE_COUNT: u32 = 64;
/// A node whose task stops is usually a mesh that was busy, so it is asked
/// again — and then skipped, so one silent node never holds up the sweep.
const ATTEMPTS_PER_NODE: usize = 3;
/// Tasks in flight at once. The border router runs them one at a time anyway;
/// this bounds how many are queued on it.
const MAX_CONCURRENT: usize = 4;

/// What every node is asked for: the types `translate_node` decodes, named as
/// ot-br-posix names them. The border router recognises 27 in total and
/// rejects a whole task for one name it does not know, so this list is worth
/// keeping exact. The three "query" types (children, their addresses, router
/// neighbours) are left out: each costs the border router a further round of
/// mesh traffic, and nothing here reads them.
const DIAGNOSTIC_TYPES: &[&str] = &[
    "extAddress",
    "rloc16",
    "mode",
    "timeout",
    "connectivity",
    "route",
    "leaderData",
    "networkData",
    "ipv6Addresses",
    "macCounters",
    "batteryLevel",
    "supplyVoltage",
    "childTable",
    "channelPages",
    "maxChildTimeout",
    "eui",
    "threadVersion",
    "vendorName",
    "vendorModel",
    "vendorSwVersion",
    "threadStackVersion",
    "mleCounters",
];

/// Why a REST exchange failed, in the two ways the protocol tells apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestError {
    /// Nothing answered: refused connection, timeout, a reset mid-body.
    Unreachable(String),
    /// Something answered, and it was not what a border router says: a
    /// non-2xx status, a body that is not JSON, or JSON of the wrong shape.
    Protocol(String),
}

impl RestError {
    /// The reason a batch carries when this ends a collection.
    pub fn partial_reason(&self) -> ThreadDiagnosticsPartial {
        match self {
            RestError::Unreachable(_) => ThreadDiagnosticsPartial::RestUnreachable,
            RestError::Protocol(_) => ThreadDiagnosticsPartial::RestProtocol,
        }
    }
}

impl fmt::Display for RestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestError::Unreachable(detail) => write!(f, "border router unreachable: {}", detail),
            RestError::Protocol(detail) => {
                write!(f, "border router answered unexpectedly: {}", detail)
            }
        }
    }
}

/// Which diagnostics API a border router serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticsApi {
    /// The task collection under `/api/actions` and `/api/diagnostics`.
    Collection,
    /// The one-shot `GET /diagnostics` of older builds.
    Legacy,
    /// REST is served, but neither: this border router cannot collect for us.
    None,
}

/// What probing one border router's REST endpoint found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub base_url: String,
    pub network_name: String,
    /// Uppercase hex, as the protocol spells it.
    pub ext_pan_id_hex: String,
    pub diagnostics_api: DiagnosticsApi,
}

/// How a settled task ended, and where its result is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionOutcome {
    /// `completed`, `stopped` (the mesh-side timeout) or `failed`.
    pub status: String,
    /// The diagnostics item a completed task produced.
    pub result_id: Option<String>,
}

/// A client bound to one border router.
pub struct Client {
    base_url: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(host: IpAddr, port: u16) -> Self {
        let base_url = match host {
            IpAddr::V4(host) => format!("http://{}:{}", host, port),
            IpAddr::V6(host) => format!("http://[{}]:{}", host, port),
        };
        Self::for_base_url(&base_url)
    }

    pub fn for_base_url(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new().build(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether this is a border router's REST API, and which one.
    ///
    /// `/node` is served by every build and names the network, so it both
    /// confirms what is answering and says which network it is on. A status
    /// probe of the two diagnostics paths then says which API to speak.
    pub fn probe(&self) -> Result<Capability, RestError> {
        let node = self.get_json("/node", "application/json", PROBE_TIMEOUT)?;
        let node = node
            .as_object()
            .ok_or_else(|| RestError::Protocol("/node did not return an object".into()))?;
        let network_name = string(node, "networkName")
            .ok_or_else(|| RestError::Protocol("/node has no networkName".into()))?;
        let ext_pan_id_hex = string(node, "extPanId")
            .and_then(|hex| hex_bytes(&hex, Some(8)))
            .map(|bytes| to_hex(&bytes))
            .ok_or_else(|| RestError::Protocol("/node has no 8-byte extPanId".into()))?;

        let diagnostics_api = if self.answers("/api/diagnostics")? {
            DiagnosticsApi::Collection
        } else if self.answers("/diagnostics")? {
            DiagnosticsApi::Legacy
        } else {
            DiagnosticsApi::None
        };

        Ok(Capability {
            base_url: self.base_url.clone(),
            network_name,
            ext_pan_id_hex,
            diagnostics_api,
        })
    }

    /// Whether a path answers 2xx. Any other status is a plain no — a 404 is
    /// how a build says it lacks an endpoint — and only a transport failure is
    /// an error.
    fn answers(&self, path: &str) -> Result<bool, RestError> {
        match self
            .agent
            .get(&format!("{}{}", self.base_url, path))
            .set("Accept", "application/json")
            .timeout(PROBE_TIMEOUT)
            .call()
        {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(_, _)) => Ok(false),
            Err(error) => Err(RestError::Unreachable(format!("GET {}: {}", path, error))),
        }
    }

    /// GET a path as JSON with its keys normalised. `Null` for `204`.
    fn get_json(&self, path: &str, accept: &str, timeout: Duration) -> Result<Value, RestError> {
        let response = self
            .agent
            .get(&format!("{}{}", self.base_url, path))
            .set("Accept", accept)
            .timeout(timeout)
            .call()
            .map_err(|error| transport_error("GET", path, error))?;
        if response.status() == 204 {
            return Ok(Value::Null);
        }
        let text = response
            .into_string()
            .map_err(|error| RestError::Unreachable(format!("GET {}: {}", path, error)))?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|error| RestError::Protocol(format!("GET {} is not JSON: {}", path, error)))?;
        Ok(normalize_keys(&parsed))
    }

    /// Enqueue one task and hand back the id the border router gave it.
    pub fn post_action(&self, kind: &str, attributes: Value) -> Result<String, RestError> {
        let path = "/api/actions";
        let body = json!({ "data": [{ "type": kind, "attributes": attributes }] });
        let response = self
            .agent
            .post(&format!("{}{}", self.base_url, path))
            .set("Content-Type", "application/vnd.api+json")
            .set("Accept", "application/vnd.api+json")
            .timeout(REQUEST_TIMEOUT)
            .send_string(&body.to_string())
            .map_err(|error| transport_error("POST", path, error))?;
        let text = response
            .into_string()
            .map_err(|error| RestError::Unreachable(format!("POST {}: {}", path, error)))?;
        let parsed: Value = serde_json::from_str(&text).map_err(|error| {
            RestError::Protocol(format!("POST {} is not JSON: {}", path, error))
        })?;
        let parsed = normalize_keys(&parsed);
        parsed
            .get("data")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RestError::Protocol(format!("POST {} returned no task id", path)))
    }

    /// Where a task stands.
    pub fn action_status(&self, id: &str) -> Result<ActionOutcome, RestError> {
        let path = format!("/api/actions/{}", id);
        let parsed = self.get_json(&path, "application/vnd.api+json", REQUEST_TIMEOUT)?;
        // JSON:API wraps the item in `data`; a build answering the plain
        // media type may not.
        let item = parsed.get("data").unwrap_or(&parsed);
        let status = item
            .get("attributes")
            .and_then(|attributes| attributes.get("status"))
            .or_else(|| item.get("status"))
            .and_then(Value::as_str)
            .ok_or_else(|| RestError::Protocol(format!("GET {} has no status", path)))?
            .to_string();
        let result_id = item
            .get("relationships")
            .and_then(|relationships| relationships.get("result"))
            .and_then(|result| result.get("data"))
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(ActionOutcome { status, result_id })
    }

    /// Poll a task until it settles, or until `timeout` passes without it.
    ///
    /// A terminal status seen on a poll is always honoured, however late the
    /// poll: the work is done, and discarding it would only mean asking again.
    pub fn settle_action(&self, id: &str, timeout: Duration) -> Result<ActionOutcome, RestError> {
        let deadline = Instant::now() + timeout;
        loop {
            let outcome = self.action_status(id)?;
            if matches!(outcome.status.as_str(), "completed" | "stopped" | "failed") {
                return Ok(outcome);
            }
            if Instant::now() >= deadline {
                return Err(RestError::Protocol(format!(
                    "task {} still {} after {:?}",
                    id, outcome.status, timeout
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Drop the border router's results so far, so a sweep reads only what it
    /// collected itself.
    pub fn clear_diagnostics(&self) -> Result<(), RestError> {
        let path = "/api/diagnostics";
        match self
            .agent
            .delete(&format!("{}{}", self.base_url, path))
            .timeout(REQUEST_TIMEOUT)
            .call()
        {
            Ok(_) => Ok(()),
            Err(error) => Err(transport_error("DELETE", path, error)),
        }
    }

    /// The device directory: every node the border router knows of.
    pub fn devices(&self) -> Result<Vec<Value>, RestError> {
        self.collection("/api/devices")
    }

    /// Every diagnostics item the border router holds.
    pub fn diagnostics_collection(&self) -> Result<Vec<Value>, RestError> {
        self.collection("/api/diagnostics")
    }

    /// One diagnostics item, as a completed task names it.
    pub fn diagnostic(&self, id: &str) -> Result<Value, RestError> {
        let parsed = self.get_json(
            &format!("/api/diagnostics/{}", id),
            "application/vnd.api+json",
            REQUEST_TIMEOUT,
        )?;
        Ok(match parsed.get("data") {
            Some(item) if item.is_object() => item.clone(),
            _ => parsed,
        })
    }

    /// The whole mesh at once, from a build that predates the collection API.
    pub fn legacy_diagnostics(&self) -> Result<Vec<Value>, RestError> {
        self.collection("/diagnostics")
    }

    /// A collection, whether served bare or in a JSON:API `data` envelope.
    fn collection(&self, path: &str) -> Result<Vec<Value>, RestError> {
        let parsed = self.get_json(path, "application/vnd.api+json", REQUEST_TIMEOUT)?;
        match parsed {
            Value::Array(items) => Ok(items),
            Value::Object(ref object) => match object.get("data") {
                Some(Value::Array(items)) => Ok(items.clone()),
                _ => Err(RestError::Protocol(format!(
                    "GET {} did not return a collection",
                    path
                ))),
            },
            // A 204: nothing collected yet.
            Value::Null => Ok(Vec::new()),
            _ => Err(RestError::Protocol(format!(
                "GET {} did not return a collection",
                path
            ))),
        }
    }
}

fn transport_error(method: &str, path: &str, error: ureq::Error) -> RestError {
    match error {
        ureq::Error::Status(status, _) => {
            RestError::Protocol(format!("{} {} returned {}", method, path, status))
        }
        other => RestError::Unreachable(format!("{} {}: {}", method, path, other)),
    }
}

/// Collect the whole mesh, handing over each node as it is decoded.
///
/// Nodes arrive in no particular order and a node that will not answer is
/// skipped, not fatal: a partial mesh view is a valid one, and the border
/// router's own UI behaves the same. What *is* fatal is losing the border
/// router itself, or it answering in a shape this does not understand.
pub fn collect(
    client: &Client,
    capability: &Capability,
    on_node: &(dyn Fn(ThreadDiagnosticsNode) + Sync),
) -> Result<(), RestError> {
    match capability.diagnostics_api {
        DiagnosticsApi::None => Err(RestError::Protocol(
            "this border router does not serve diagnostics over REST".into(),
        )),
        DiagnosticsApi::Legacy => {
            for entry in client.legacy_diagnostics()? {
                if let Some(node) = translate_item(&entry) {
                    on_node(node);
                }
            }
            Ok(())
        }
        DiagnosticsApi::Collection => collect_via_tasks(client, on_node),
    }
}

/// The collection API: refresh the device directory, then one task per
/// device, a few at a time, each node handed over as its task completes.
fn collect_via_tasks(
    client: &Client,
    on_node: &(dyn Fn(ThreadDiagnosticsNode) + Sync),
) -> Result<(), RestError> {
    // Best effort: a build that serves the read side but refuses the DELETE
    // must not fail every sweep. Stale items then sit beside fresh ones, and
    // the final pass below only reads items no task of ours produced.
    if let Err(error) = client.clear_diagnostics() {
        log::debug!("Could not clear the border router's diagnostics: {}", error);
    }

    let discovery = client.post_action(
        "updateDeviceCollectionTask",
        json!({
            "maxAge": DISCOVER_MAX_AGE_S,
            "maxRetries": DISCOVER_MAX_RETRIES,
            "deviceCount": DISCOVER_DEVICE_COUNT,
            "timeout": DISCOVER_TASK_TIMEOUT_S,
        }),
    )?;
    let outcome = client.settle_action(&discovery, DISCOVER_TIMEOUT)?;
    if outcome.status != "completed" {
        log::debug!(
            "The border router's device discovery ended {}; the directory may be incomplete",
            outcome.status
        );
    }

    let destinations: VecDeque<String> = client
        .devices()?
        .iter()
        .filter_map(|device| string(attributes_of(device)?, "extAddress"))
        .collect();
    let device_count = destinations.len();

    let queue = Mutex::new(destinations);
    let produced: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let workers = MAX_CONCURRENT.min(device_count).max(1);
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let Some(destination) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                query_node(client, &destination, on_node, &produced);
            });
        }
    });

    // Whatever the per-task reads missed — an item a retry produced after
    // the first attempt was given up on, say.
    let produced = produced.into_inner().unwrap();
    let mut missed = 0;
    for entry in client.diagnostics_collection()? {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
        if produced.contains(id) {
            continue;
        }
        if let Some(node) = translate_item(&entry) {
            missed += 1;
            on_node(node);
        }
    }
    if device_count > 0 && produced.is_empty() && missed == 0 {
        // An empty mesh and a mesh nothing answered for look the same in the
        // batch; the log tells them apart.
        log::warn!(
            "The border router lists {} device(s), but none answered a diagnostic query",
            device_count
        );
    }
    Ok(())
}

/// One node's task, tried a few times, its result handed over on completion.
fn query_node(
    client: &Client,
    destination: &str,
    on_node: &(dyn Fn(ThreadDiagnosticsNode) + Sync),
    produced: &Mutex<BTreeSet<String>>,
) {
    for attempt in 1..=ATTEMPTS_PER_NODE {
        let settled = client
            .post_action(
                "getNetworkDiagnosticTask",
                json!({
                    "destination": destination,
                    "types": DIAGNOSTIC_TYPES,
                    "timeout": DIAG_TASK_TIMEOUT_S,
                }),
            )
            .and_then(|id| client.settle_action(&id, ACTION_TIMEOUT));
        match settled {
            Ok(outcome) if outcome.status == "completed" => {
                let Some(result_id) = outcome.result_id else {
                    log::debug!("Node {} completed with no result item", destination);
                    return;
                };
                match client.diagnostic(&result_id) {
                    Ok(item) => {
                        produced.lock().unwrap().insert(result_id);
                        if let Some(node) = translate_item(&item) {
                            on_node(node);
                        }
                    }
                    Err(error) => {
                        log::debug!("Node {}'s result could not be read: {}", destination, error)
                    }
                }
                return;
            }
            Ok(outcome) => log::debug!(
                "Node {} ended {} (attempt {}/{})",
                destination,
                outcome.status,
                attempt,
                ATTEMPTS_PER_NODE
            ),
            Err(error) => log::debug!(
                "Node {} could not be queried (attempt {}/{}): {}",
                destination,
                attempt,
                ATTEMPTS_PER_NODE,
                error
            ),
        }
    }
}

/// A collection item is `{id, type, attributes: {…}}`; a legacy entry is the
/// bare node. Either way it is the node that gets translated.
fn attributes_of(item: &Value) -> Option<&Map<String, Value>> {
    let object = item.as_object()?;
    Some(match object.get("attributes") {
        Some(Value::Object(attributes))
            if object.contains_key("type") || object.contains_key("id") =>
        {
            attributes
        }
        _ => object,
    })
}

/// One item of either API as a node, or `None` for an item that identifies no
/// node at all.
pub fn translate_item(item: &Value) -> Option<ThreadDiagnosticsNode> {
    let normalised = normalize_keys(item);
    let node = translate_node(attributes_of(&normalised)?);
    (node.ext_mac_address.is_some() || node.rloc16.is_some()).then_some(node)
}

/// Read a border router's JSON for one node into the protocol's shape.
///
/// Every field is optional and each is read on its own: REST hands back
/// whatever the border router last collected, which differs per node and per
/// build, and one malformed field must not cost the node. Where the current
/// and the legacy build spell a key differently, both spellings are read.
pub fn translate_node(node: &Map<String, Value>) -> ThreadDiagnosticsNode {
    ThreadDiagnosticsNode {
        ext_mac_address: string(node, "extAddress")
            .and_then(|hex| hex_bytes(&hex, Some(8)))
            .map(|bytes| to_hex(&bytes)),
        rloc16: node.get("rloc16").and_then(rloc16),
        mode: node.get("mode").and_then(Value::as_object).and_then(mode),
        timeout: number(node, "timeout"),
        connectivity: node
            .get("connectivity")
            .and_then(Value::as_object)
            .and_then(connectivity),
        route64: node.get("route").and_then(Value::as_object).and_then(route),
        leader_data: node
            .get("leaderData")
            .and_then(Value::as_object)
            .and_then(leader_data),
        network_data: string(node, "networkData")
            .and_then(|hex| hex_bytes(&hex, None))
            .map(|bytes| to_hex(&bytes)),
        ipv6_addresses: node
            .get("ipv6Addresses")
            .or_else(|| node.get("ip6AddressList"))
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter()
                    .map(|entry| {
                        entry
                            .as_str()
                            .and_then(|text| text.parse::<Ipv6Addr>().ok())
                            .map(|address| to_hex(&address.octets()))
                    })
                    .collect()
            }),
        mac_counters: node
            .get("macCounters")
            .and_then(Value::as_object)
            .and_then(mac_counters),
        child_table: node
            .get("childTable")
            .and_then(Value::as_array)
            .and_then(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.as_object().and_then(child))
                    .collect()
            }),
        channel_pages: node.get("channelPages").and_then(|pages| match pages {
            Value::String(hex) => hex_bytes(hex, None),
            Value::Array(list) => list
                .iter()
                .map(|page| u8::try_from(page.as_u64()?).ok())
                .collect(),
            _ => None,
        }),
        max_child_timeout: number(node, "maxChildTimeout"),
        eui64: string(node, "eui")
            .or_else(|| string(node, "eui64"))
            .and_then(|hex| hex_bytes(&hex, Some(8)))
            .map(|bytes| to_hex(&bytes)),
        version: number(node, "threadVersion").or_else(|| number(node, "version")),
        vendor_name: string(node, "vendorName"),
        vendor_model: string(node, "vendorModel"),
        vendor_sw_version: string(node, "vendorSwVersion"),
        thread_stack_version: string(node, "threadStackVersion")
            .or_else(|| string(node, "stackVersion")),
        vendor_app_url: string(node, "vendorAppUrl"),
        mle_counters: node
            .get("mleCounters")
            .and_then(Value::as_object)
            .and_then(mle_counters),
        battery_level: number(node, "batteryLevel"),
        supply_voltage: number(node, "supplyVoltage"),
        // REST decodes every TLV it knows and drops the rest; nothing raw
        // reaches here.
        unknown: None,
    }
}

/// The current build says `fullThreadDevice` and `fullNetworkData` as
/// booleans; the legacy one said `deviceType` and `networkData` as numbers.
fn mode(object: &Map<String, Value>) -> Option<ThreadMode> {
    Some(ThreadMode {
        rx_on_when_idle: flag(object, &["rxOnWhenIdle"])?,
        ftd: flag(object, &["fullThreadDevice", "deviceType"])?,
        full_network_data: flag(object, &["fullNetworkData", "networkData"])?,
    })
}

fn connectivity(object: &Map<String, Value>) -> Option<ThreadConnectivity> {
    Some(ThreadConnectivity {
        parent_priority: object
            .get("parentPriority")
            .and_then(Value::as_i64)
            .map(|priority| priority.signum() as i8)?,
        link_quality3: number(object, "linkQuality3")?,
        link_quality2: number(object, "linkQuality2")?,
        link_quality1: number(object, "linkQuality1")?,
        leader_cost: number(object, "leaderCost")?,
        id_sequence: number(object, "idSequence")?,
        active_routers: number(object, "activeRouters")?,
        sed_buffer_size: number(object, "sedBufferSize")?,
        sed_datagram_count: number(object, "sedDatagramCount")?,
    })
}

fn route(object: &Map<String, Value>) -> Option<ThreadRoute64> {
    let entries = object
        .get("routeData")
        .and_then(Value::as_array)?
        .iter()
        .map(|entry| {
            let entry = entry.as_object()?;
            Some(ThreadRoute64Entry {
                router_id: number(entry, "routeId").or_else(|| number(entry, "routerId"))?,
                link_quality_in: number(entry, "linkQualityIn")?,
                link_quality_out: number(entry, "linkQualityOut")?,
                route_cost: number(entry, "routeCost")?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ThreadRoute64 {
        id_sequence: number(object, "idSequence")?,
        entries,
    })
}

fn leader_data(object: &Map<String, Value>) -> Option<ThreadLeaderData> {
    Some(ThreadLeaderData {
        partition_id: number(object, "partitionId")?,
        weighting: number(object, "weighting")?,
        data_version: number(object, "dataVersion")?,
        stable_data_version: number(object, "stableDataVersion")?,
        leader_router_id: number(object, "leaderRouterId")?,
    })
}

fn mac_counters(object: &Map<String, Value>) -> Option<ThreadMacCounters> {
    Some(ThreadMacCounters {
        if_in_unknown_protos: number(object, "ifInUnknownProtos")?,
        if_in_errors: number(object, "ifInErrors")?,
        if_out_errors: number(object, "ifOutErrors")?,
        if_in_ucast_pkts: number(object, "ifInUcastPkts")?,
        if_in_broadcast_pkts: number(object, "ifInBroadcastPkts")?,
        if_in_discards: number(object, "ifInDiscards")?,
        if_out_ucast_pkts: number(object, "ifOutUcastPkts")?,
        if_out_broadcast_pkts: number(object, "ifOutBroadcastPkts")?,
        if_out_discards: number(object, "ifOutDiscards")?,
    })
}

/// The border router names these differently from the TLV they came from:
/// `radioDisabledCount` for the disabled-role count, `totalTrackingTime` for
/// the tracked time, and so on. The times are passed through as the border
/// router reports them.
fn mle_counters(object: &Map<String, Value>) -> Option<ThreadMleCounters> {
    let time = |key: &str| -> Option<u64> {
        match object.get(key)? {
            Value::Number(number) => number.as_u64().or_else(|| {
                number
                    .as_f64()
                    .filter(|value| *value >= 0.0)
                    .map(|value| value as u64)
            }),
            Value::String(text) => text.trim().parse().ok(),
            _ => None,
        }
    };
    Some(ThreadMleCounters {
        disabled_role: number(object, "radioDisabledCount")?,
        detached_role: number(object, "detachedRoleCount")?,
        child_role: number(object, "childRoleCount")?,
        router_role: number(object, "routerRoleCount")?,
        leader_role: number(object, "leaderRoleCount")?,
        attach_attempts: number(object, "attachAttemptsCount")?,
        partition_id_changes: number(object, "partIdChangesCount")?,
        better_partition_attach_attempts: number(object, "betterPartIdAttachAttemptsCount")?,
        parent_changes: number(object, "newParentCount")?,
        tracked_time: time("totalTrackingTime")?,
        disabled_time: time("radioDisabledTime")?,
        detached_time: time("detachedRoleTime")?,
        child_time: time("childRoleTime")?,
        router_time: time("routerRoleTime")?,
        leader_time: time("leaderRoleTime")?,
    })
}

/// A child's timeout is carried as an exponent: `2^(exp - 4)` seconds, with
/// exponents below 4 clamped to 4, as OpenThread itself resolves it. The
/// legacy build omitted the link quality; it reads as 0 there.
fn child(object: &Map<String, Value>) -> Option<ThreadChildTableEntry> {
    let timeout_exponent: u8 = number(object, "timeout")?;
    let clamped = timeout_exponent.clamp(4, 35);
    Some(ThreadChildTableEntry {
        timeout_exponent,
        timeout_seconds: 1u32.checked_shl(u32::from(clamped) - 4).unwrap_or(u32::MAX),
        incoming_link_quality: number(object, "linkQuality").unwrap_or(0),
        child_id: number(object, "childId")?,
        mode: object
            .get("mode")
            .and_then(Value::as_object)
            .and_then(mode)?,
    })
}

/// A boolean the border router may spell as a bool or as 0/1.
fn flag(object: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| match object.get(*key)? {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => Some(value.as_i64()? != 0),
        _ => None,
    })
}

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_string)
}

/// An integer field, in whatever width the protocol wants for it.
fn number<T: TryFrom<u64>>(object: &Map<String, Value>, key: &str) -> Option<T> {
    T::try_from(object.get(key)?.as_u64()?).ok()
}

/// The current build spells an RLOC16 `"0x5000"`; the legacy one sent a number.
fn rloc16(value: &Value) -> Option<u16> {
    match value {
        Value::Number(number) => u16::try_from(number.as_u64()?).ok(),
        Value::String(text) => {
            let text = text.trim();
            match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                Some(hex) => u16::from_str_radix(hex, 16).ok(),
                None => text.parse().ok(),
            }
        }
        _ => None,
    }
}

/// Bytes from a hex string, of `expected` length when one is given.
fn hex_bytes(text: &str, expected: Option<usize>) -> Option<Vec<u8>> {
    let text = text.trim();
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    if expected.is_some_and(|expected| text.len() != expected * 2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{:02X}", byte)).collect()
}

/// Rewrite every object key to camelCase, recursively.
///
/// Legacy builds emit PascalCase (`NetworkName`, `IP6AddressList`); current
/// ones camelCase. Lower-casing the first letter is right for all but a few
/// acronym-led keys, which are mapped by name — the same table
/// python-otbr-api keeps. Idempotent, so a current build's answer passes
/// through unchanged.
pub fn normalize_keys(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(normalize_keys).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (normalize_key(key), normalize_keys(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn normalize_key(key: &str) -> String {
    match key {
        "BaId" => "baId".to_string(),
        "IP6AddressList" => "ip6AddressList".to_string(),
        "MACCounters" => "macCounters".to_string(),
        "PSKc" => "pskc".to_string(),
        _ => {
            let mut chars = key.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

/// A border router that answers scripted responses, for the tests here and
/// in `api::thread_diagnostics`.
#[cfg(test)]
pub mod mock {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    /// One request as the mock saw it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Seen {
        pub method: String,
        pub path: String,
        pub body: String,
    }

    pub type Handler = dyn Fn(&Seen) -> (u16, String) + Send + Sync + 'static;

    pub struct MockBorderRouter {
        pub port: u16,
        pub base_url: String,
        pub seen: Arc<Mutex<Vec<Seen>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl MockBorderRouter {
        pub fn start(handler: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let handler: Arc<Handler> = Arc::new(handler);
            let thread = {
                let seen = seen.clone();
                let stop = stop.clone();
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream.set_nonblocking(false).unwrap();
                                serve(stream, handler.as_ref(), &seen);
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                port,
                base_url: format!("http://127.0.0.1:{}", port),
                seen,
                stop,
                thread: Some(thread),
            }
        }

        pub fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for MockBorderRouter {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn serve(stream: TcpStream, handler: &Handler, seen: &Mutex<Vec<Seen>>) {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
            return;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            let _ = reader.read_exact(&mut body);
        }
        let request = Seen {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
        };
        let (status, response_body) = handler(&request);
        seen.lock().unwrap().push(request);
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            404 => "Not Found",
            422 => "Unprocessable Content",
            _ => "Status",
        };
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/vnd.api+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status,
            reason,
            response_body.len()
        );
        let mut stream = reader.into_inner();
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(response_body.as_bytes());
        let _ = stream.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{MockBorderRouter, Seen};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A node as the current build's collection API renders it: the example
    /// in ot-br-posix's own OpenAPI file, trimmed to what this reads.
    const COLLECTION_ITEM: &str = r#"{
        "id": "fd428d18-8528-45cf-8a2d-f191bef39b5d",
        "type": "threadNetworkDiagnostic",
        "attributes": {
            "created": "2023-11-23T18:30:42Z",
            "extAddress": "96518e5497d5b9f3",
            "rloc16": "0x5000",
            "mode": { "rxOnWhenIdle": true, "fullThreadDevice": true, "fullNetworkData": true },
            "leaderData": { "partitionId": 1794764107, "weighting": 64, "dataVersion": 64,
                            "stableDataVersion": 230, "leaderRouterId": 60 },
            "ipv6Addresses": ["fdd2:399d:3564:aba6:36f1:b05d:d638:43b6", "fd11:22::aadb:cb2d:47b9:97bc"],
            "macCounters": { "ifInUnknownProtos": 0, "ifInErrors": 0, "ifOutErrors": 0,
                             "ifInUcastPkts": 9, "ifInBroadcastPkts": 1167, "ifInDiscards": 0,
                             "ifOutUcastPkts": 6, "ifOutBroadcastPkts": 926, "ifOutDiscards": 0 },
            "batteryLevel": null,
            "supplyVoltage": 5000,
            "channelPages": [0],
            "maxChildTimeout": 2880,
            "eui": "9035eafffef02ca9",
            "threadVersion": 4,
            "vendorName": "VendorName",
            "vendorModel": "VendorModel",
            "vendorSwVersion": "1.0.0",
            "threadStackVersion": "openthread-esp32/-; esp32; Nov 1 2023 01:02:03",
            "childTable": [
                { "childId": 3, "timeout": 8, "linkQuality": 3,
                  "mode": { "rxOnWhenIdle": false, "fullThreadDevice": false, "fullNetworkData": true } }
            ],
            "route": { "idSequence": 12, "routeData": [
                { "routeId": 20, "linkQualityOut": 3, "linkQualityIn": 2, "routeCost": 1 } ] },
            "mleCounters": { "radioDisabledCount": 1, "detachedRoleCount": 1, "childRoleCount": 1,
                             "routerRoleCount": 1, "leaderRoleCount": 0, "attachAttemptsCount": 1,
                             "partIdChangesCount": 0, "betterPartIdAttachAttemptsCount": 0,
                             "newParentCount": 1, "totalTrackingTime": 44000.0,
                             "radioDisabledTime": 21.434, "detachedRoleTime": 174.56,
                             "childRoleTime": 62.006, "routerRoleTime": 43742.0,
                             "leaderRoleTime": 0.0 }
        }
    }"#;

    /// The same node as an older build's `GET /diagnostics` spelled it.
    const LEGACY_ITEM: &str = r#"{
        "ExtAddress": "2a55d952bc7b4008",
        "Rloc16": 20484,
        "Mode": { "RxOnWhenIdle": 0, "DeviceType": 0, "NetworkData": 1 },
        "Timeout": 240,
        "Connectivity": { "ParentPriority": -1, "LinkQuality3": 2, "LinkQuality2": 1,
                          "LinkQuality1": 0, "LeaderCost": 3, "IdSequence": 12,
                          "ActiveRouters": 4, "SedBufferSize": 1280, "SedDatagramCount": 1 },
        "IP6AddressList": ["fe80::1"],
        "NetworkData": "0801fd00",
        "ChannelPages": "00",
        "ChildTable": [ { "ChildId": 1, "Timeout": 2,
                          "Mode": { "RxOnWhenIdle": 1, "DeviceType": 1, "NetworkData": 1 } } ],
        "MACCounters": { "IfInUnknownProtos": 0, "IfInErrors": 0, "IfOutErrors": 0,
                         "IfInUcastPkts": 1, "IfInBroadcastPkts": 2, "IfInDiscards": 0,
                         "IfOutUcastPkts": 3, "IfOutBroadcastPkts": 4, "IfOutDiscards": 0 }
    }"#;

    #[test]
    fn a_collection_item_translates_field_by_field() {
        let node = translate_item(&serde_json::from_str(COLLECTION_ITEM).unwrap()).unwrap();
        assert_eq!(node.ext_mac_address.as_deref(), Some("96518E5497D5B9F3"));
        assert_eq!(node.rloc16, Some(0x5000));
        let mode = node.mode.unwrap();
        assert!(mode.rx_on_when_idle && mode.ftd && mode.full_network_data);
        assert_eq!(node.leader_data.unwrap().partition_id, 1794764107);
        assert_eq!(
            node.ipv6_addresses,
            Some(vec![
                "FDD2399D3564ABA636F1B05DD63843B6".to_string(),
                "FD11002200000000AADBCB2D47B997BC".to_string(),
            ])
        );
        assert_eq!(node.mac_counters.unwrap().if_in_broadcast_pkts, 1167);
        assert_eq!(node.battery_level, None);
        assert_eq!(node.supply_voltage, Some(5000));
        assert_eq!(node.channel_pages, Some(vec![0]));
        assert_eq!(node.max_child_timeout, Some(2880));
        assert_eq!(node.eui64.as_deref(), Some("9035EAFFFEF02CA9"));
        assert_eq!(node.version, Some(4));
        assert_eq!(node.vendor_name.as_deref(), Some("VendorName"));
        assert_eq!(
            node.thread_stack_version.as_deref(),
            Some("openthread-esp32/-; esp32; Nov 1 2023 01:02:03")
        );
        let child = node.child_table.unwrap()[0];
        assert_eq!(child.child_id, 3);
        assert_eq!(child.timeout_exponent, 8);
        assert_eq!(child.timeout_seconds, 16);
        assert_eq!(child.incoming_link_quality, 3);
        assert!(!child.mode.rx_on_when_idle && child.mode.full_network_data);
        let route = node.route64.unwrap();
        assert_eq!(route.id_sequence, 12);
        assert_eq!(route.entries[0].router_id, 20);
        assert_eq!(route.entries[0].link_quality_in, 2);
        let mle = node.mle_counters.unwrap();
        assert_eq!(mle.disabled_role, 1);
        assert_eq!(mle.parent_changes, 1);
        assert_eq!(mle.tracked_time, 44000);
        assert_eq!(mle.detached_time, 174);
        assert!(node.unknown.is_none());
    }

    #[test]
    fn a_legacy_item_is_read_through_its_older_spelling() {
        let node = translate_item(&serde_json::from_str(LEGACY_ITEM).unwrap()).unwrap();
        assert_eq!(node.ext_mac_address.as_deref(), Some("2A55D952BC7B4008"));
        assert_eq!(node.rloc16, Some(0x5004));
        let mode = node.mode.unwrap();
        assert!(!mode.rx_on_when_idle && !mode.ftd && mode.full_network_data);
        assert_eq!(node.timeout, Some(240));
        let connectivity = node.connectivity.unwrap();
        assert_eq!(connectivity.parent_priority, -1);
        assert_eq!(connectivity.sed_buffer_size, 1280);
        assert_eq!(
            node.ipv6_addresses,
            Some(vec!["FE800000000000000000000000000001".to_string()])
        );
        assert_eq!(node.network_data.as_deref(), Some("0801FD00"));
        assert_eq!(node.channel_pages, Some(vec![0]));
        let child = node.child_table.unwrap()[0];
        // Below the floor: clamped to 4, so one second.
        assert_eq!(child.timeout_exponent, 2);
        assert_eq!(child.timeout_seconds, 1);
        assert_eq!(child.incoming_link_quality, 0);
        assert_eq!(node.mac_counters.unwrap().if_out_broadcast_pkts, 4);
    }

    #[test]
    fn a_malformed_field_costs_only_itself() {
        let item = json!({
            "extAddress": "96518e5497d5b9f3",
            "rloc16": "not a number",
            "leaderData": { "partitionId": "wrong" },
            "ipv6Addresses": ["fd00::1", "not an address"],
            "mode": { "rxOnWhenIdle": true },
            "vendorName": "Still here"
        });
        let node = translate_item(&item).unwrap();
        assert!(node.ext_mac_address.is_some());
        assert!(node.rloc16.is_none());
        assert!(node.leader_data.is_none());
        assert!(node.ipv6_addresses.is_none());
        assert!(node.mode.is_none());
        assert_eq!(node.vendor_name.as_deref(), Some("Still here"));

        // Nothing identifying it: not a node.
        assert!(translate_item(&json!({ "vendorName": "ghost" })).is_none());
        assert!(translate_item(&json!("not an object")).is_none());
    }

    #[test]
    fn keys_are_normalised_the_way_python_otbr_api_does_it() {
        let value = json!({ "NetworkName": "n", "IP6AddressList": [], "MACCounters": {},
                            "BaId": "x", "PSKc": "y", "Nested": { "ExtAddress": "z" },
                            "alreadyCamel": 1 });
        let normalised = normalize_keys(&value);
        let keys: Vec<&String> = normalised.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "alreadyCamel",
                "baId",
                "ip6AddressList",
                "macCounters",
                "nested",
                "networkName",
                "pskc"
            ]
        );
        assert_eq!(normalised["nested"]["extAddress"], json!("z"));
        assert_eq!(normalize_keys(&normalised), normalised);
    }

    fn node_json() -> String {
        json!({ "networkName": "MyThreadNet", "extPanId": "1122334455667788",
                "rloc16": "0xf000", "state": "leader" })
        .to_string()
    }

    #[test]
    fn a_probe_names_the_network_and_the_api_it_serves() {
        let router = MockBorderRouter::start(|request| match request.path.as_str() {
            "/node" => (200, node_json()),
            "/api/diagnostics" => (200, r#"{"data":[]}"#.into()),
            _ => (404, String::new()),
        });
        let capability = Client::for_base_url(&router.base_url).probe().unwrap();
        assert_eq!(capability.network_name, "MyThreadNet");
        assert_eq!(capability.ext_pan_id_hex, "1122334455667788");
        assert_eq!(capability.diagnostics_api, DiagnosticsApi::Collection);
        assert_eq!(capability.base_url, router.base_url);

        let legacy = MockBorderRouter::start(|request| match request.path.as_str() {
            "/node" => (200, node_json()),
            "/diagnostics" => (200, "[]".into()),
            _ => (404, String::new()),
        });
        let capability = Client::for_base_url(&legacy.base_url).probe().unwrap();
        assert_eq!(capability.diagnostics_api, DiagnosticsApi::Legacy);

        let bare = MockBorderRouter::start(|request| match request.path.as_str() {
            "/node" => (200, node_json()),
            _ => (404, String::new()),
        });
        let capability = Client::for_base_url(&bare.base_url).probe().unwrap();
        assert_eq!(capability.diagnostics_api, DiagnosticsApi::None);
    }

    #[test]
    fn something_that_is_not_a_border_router_does_not_probe_as_one() {
        let web = MockBorderRouter::start(|_| (200, "<html>hello</html>".into()));
        assert!(matches!(
            Client::for_base_url(&web.base_url).probe(),
            Err(RestError::Protocol(_))
        ));
        let nothing = MockBorderRouter::start(|_| (404, String::new()));
        assert!(matches!(
            Client::for_base_url(&nothing.base_url).probe(),
            Err(RestError::Protocol(_))
        ));
        // Nothing listening at all.
        let error = Client::for_base_url("http://127.0.0.1:9")
            .probe()
            .unwrap_err();
        assert!(matches!(error, RestError::Unreachable(_)));
        assert_eq!(
            error.partial_reason(),
            ThreadDiagnosticsPartial::RestUnreachable
        );
    }

    /// A border router speaking the collection API, scripted end to end: the
    /// device refresh, one task per device settling after one `pending` poll,
    /// and the items those tasks point at.
    fn collection_router() -> MockBorderRouter {
        let tasks = Arc::new(AtomicUsize::new(0));
        MockBorderRouter::start(move |request: &Seen| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/node") => (200, node_json()),
                ("DELETE", "/api/diagnostics") => (204, String::new()),
                ("POST", "/api/actions") => {
                    let body: Value = serde_json::from_str(&request.body).unwrap();
                    let kind = body["data"][0]["type"].as_str().unwrap().to_string();
                    let destination = body["data"][0]["attributes"]["destination"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let n = tasks.fetch_add(1, Ordering::Relaxed);
                    // The task id carries what it is for, so the poll can
                    // answer without state.
                    let id = format!("task-{}-{}-{}", n, kind, destination);
                    (
                        200,
                        json!({ "data": [{ "type": kind, "id": id }] }).to_string(),
                    )
                }
                ("GET", path) if path.starts_with("/api/actions/task-") => {
                    let id = path.trim_start_matches("/api/actions/");
                    let destination = id.rsplit('-').next().unwrap_or("");
                    if id.contains("updateDeviceCollectionTask") {
                        return (
                            200,
                            json!({ "data": { "id": id, "type": "updateDeviceCollectionTask",
                                     "attributes": { "status": "completed" } } })
                            .to_string(),
                        );
                    }
                    let status = if destination == "dead000000000000" {
                        "stopped"
                    } else {
                        "completed"
                    };
                    let mut item = json!({ "data": { "id": id, "type": "getNetworkDiagnosticTask",
                                            "attributes": { "status": status } } });
                    if status == "completed" {
                        item["data"]["relationships"] = json!({ "result": { "data": { "type": "diagnostics",
                                                           "id": format!("diag-{}", destination) } } });
                    }
                    (200, item.to_string())
                }
                ("GET", "/api/devices") => (
                    200,
                    json!({ "data": [
                        { "id": "96518e5497d5b9f3", "type": "threadBorderRouter",
                          "attributes": { "extAddress": "96518e5497d5b9f3", "role": "leader" } },
                        { "id": "2a55d952bc7b4008", "type": "threadDevice",
                          "attributes": { "extAddress": "2a55d952bc7b4008", "role": "child" } },
                        { "id": "dead000000000000", "type": "threadDevice",
                          "attributes": { "extAddress": "dead000000000000", "role": "child" } }
                    ] })
                    .to_string(),
                ),
                ("GET", path) if path.starts_with("/api/diagnostics/diag-") => {
                    let ext = path.trim_start_matches("/api/diagnostics/diag-");
                    (
                        200,
                        json!({ "data": { "id": format!("diag-{}", ext),
                                 "type": "threadNetworkDiagnostic",
                                 "attributes": { "extAddress": ext, "rloc16": "0x1c00",
                                                 "vendorName": "Acme" } } })
                        .to_string(),
                    )
                }
                ("GET", "/api/diagnostics") => (
                    200,
                    // One item no task of ours produced: a stale one the
                    // DELETE did not clear.
                    json!({ "data": [
                        { "id": "diag-stale", "type": "threadNetworkDiagnostic",
                          "attributes": { "extAddress": "5741133700000000", "rloc16": "0x2000" } }
                    ] })
                    .to_string(),
                ),
                _ => (404, String::new()),
            }
        })
    }

    #[test]
    fn the_collection_api_is_driven_task_by_task() {
        let router = collection_router();
        let client = Client::for_base_url(&router.base_url);
        let capability = client.probe().unwrap();
        assert_eq!(capability.diagnostics_api, DiagnosticsApi::Collection);

        let nodes: Mutex<Vec<ThreadDiagnosticsNode>> = Mutex::new(Vec::new());
        collect(&client, &capability, &|node| {
            nodes.lock().unwrap().push(node)
        })
        .unwrap();

        let mut macs: Vec<String> = nodes
            .lock()
            .unwrap()
            .iter()
            .map(|node| node.ext_mac_address.clone().unwrap())
            .collect();
        macs.sort();
        // Two nodes answered, one never did (and was asked three times), and
        // the stale item the router still held was read in the final pass.
        assert_eq!(
            macs,
            vec!["2A55D952BC7B4008", "5741133700000000", "96518E5497D5B9F3"]
        );

        let requests = router.requests();
        let posted = |kind: &str| {
            requests
                .iter()
                .filter(|r| r.method == "POST" && r.body.contains(kind))
                .count()
        };
        assert_eq!(posted("updateDeviceCollectionTask"), 1);
        assert_eq!(posted("getNetworkDiagnosticTask"), 2 + ATTEMPTS_PER_NODE);
        assert!(requests
            .iter()
            .any(|r| r.method == "DELETE" && r.path == "/api/diagnostics"));
        // Every type asked for is one the border router knows.
        let task = requests
            .iter()
            .find(|r| r.method == "POST" && r.body.contains("getNetworkDiagnosticTask"))
            .unwrap();
        let body: Value = serde_json::from_str(&task.body).unwrap();
        let types = body["data"][0]["attributes"]["types"].as_array().unwrap();
        assert_eq!(types.len(), DIAGNOSTIC_TYPES.len());
        assert!(types.iter().any(|t| t == "eui") && !types.iter().any(|t| t == "eui64"));
        assert_eq!(
            body["data"][0]["attributes"]["timeout"],
            json!(DIAG_TASK_TIMEOUT_S)
        );
    }

    #[test]
    fn the_legacy_api_is_one_read() {
        let router = MockBorderRouter::start(|request| match request.path.as_str() {
            "/node" => (200, node_json()),
            "/diagnostics" => (200, format!("[{}]", LEGACY_ITEM)),
            _ => (404, String::new()),
        });
        let client = Client::for_base_url(&router.base_url);
        let capability = client.probe().unwrap();
        let nodes: Mutex<Vec<ThreadDiagnosticsNode>> = Mutex::new(Vec::new());
        collect(&client, &capability, &|node| {
            nodes.lock().unwrap().push(node)
        })
        .unwrap();
        assert_eq!(nodes.lock().unwrap().len(), 1);
        assert_eq!(nodes.lock().unwrap()[0].rloc16, Some(0x5004));
    }

    #[test]
    fn losing_the_border_router_mid_sweep_is_the_unreachable_error() {
        let capability = Capability {
            base_url: "http://127.0.0.1:9".into(),
            network_name: "n".into(),
            ext_pan_id_hex: "1122334455667788".into(),
            diagnostics_api: DiagnosticsApi::Collection,
        };
        let client = Client::for_base_url(&capability.base_url);
        let error = collect(&client, &capability, &|_| {}).unwrap_err();
        assert!(matches!(error, RestError::Unreachable(_)), "{}", error);

        let none = Capability {
            diagnostics_api: DiagnosticsApi::None,
            ..capability
        };
        assert_eq!(
            collect(&client, &none, &|_| {})
                .unwrap_err()
                .partial_reason(),
            ThreadDiagnosticsPartial::RestProtocol
        );
    }
}
