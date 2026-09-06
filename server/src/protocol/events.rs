//! Server-to-client events.
//!
//! Two things matter for parity: the exact `{event, data}` shape, and *who*
//! receives each event. Older clients never subscribed to the Thread, topology
//! or WebRTC events, so those are gated behind an opt-in the client performs by
//! issuing the corresponding command — a connection that never asks never
//! receives them.

use serde::Serialize;
use serde_json::{json, Value};

use super::model::{MatterNodeData, MatterNodeEvent, NetworkTopology, ServerInfo};

/// Which connections an event may be delivered to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventClass {
    /// Delivered to every connection that issued `start_listening`.
    Always,
    /// Requires the connection to have issued a Thread command.
    ThreadDiagnostics,
    /// Requires the connection to have issued `get_network_topology`.
    NetworkTopology,
    /// Requires the connection to have issued a WebRTC command.
    WebRtc,
}

/// An event plus its delivery class.
#[derive(Clone, Debug)]
pub struct Event {
    pub class: EventClass,
    pub payload: Value,
}

impl Event {
    fn new(class: EventClass, name: &str, data: Value) -> Self {
        Self {
            class,
            payload: json!({ "event": name, "data": data }),
        }
    }

    fn always(name: &str, data: Value) -> Self {
        Self::new(EventClass::Always, name, data)
    }

    pub fn name(&self) -> &str {
        self.payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    pub fn node_added(node: &MatterNodeData) -> Self {
        Self::always("node_added", to_value(node))
    }

    pub fn node_updated(node: &MatterNodeData) -> Self {
        Self::always("node_updated", to_value(node))
    }

    pub fn node_removed(node_id: u64) -> Self {
        Self::always("node_removed", json!(node_id))
    }

    pub fn node_event(event: &MatterNodeEvent) -> Self {
        Self::always("node_event", to_value(event))
    }

    /// `[node_id, "endpoint/cluster/attribute", value]`.
    pub fn attribute_updated(node_id: u64, path: &str, value: Value) -> Self {
        Self::always("attribute_updated", json!([node_id, path, value]))
    }

    pub fn endpoint_added(node_id: u64, endpoint_id: u16) -> Self {
        Self::always(
            "endpoint_added",
            json!({ "node_id": node_id, "endpoint_id": endpoint_id }),
        )
    }

    pub fn endpoint_removed(node_id: u64, endpoint_id: u16) -> Self {
        Self::always(
            "endpoint_removed",
            json!({ "node_id": node_id, "endpoint_id": endpoint_id }),
        )
    }

    pub fn server_info_updated(info: &ServerInfo) -> Self {
        Self::always("server_info_updated", to_value(info))
    }

    pub fn server_shutdown() -> Self {
        Self::always("server_shutdown", json!({}))
    }

    pub fn thread_diagnostics_updated(batch: Value) -> Self {
        Self::new(
            EventClass::ThreadDiagnostics,
            "thread_diagnostics_updated",
            batch,
        )
    }

    pub fn network_topology_updated(topology: &NetworkTopology) -> Self {
        Self::new(
            EventClass::NetworkTopology,
            "network_topology_updated",
            to_value(topology),
        )
    }

    pub fn webrtc_callback(data: Value) -> Self {
        Self::new(EventClass::WebRtc, "webrtc_callback", data)
    }
}

/// Wire models are infallible to serialize; a failure here would mean a model
/// with a non-string map key, which the type system already rules out.
fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// What a connection has opted into. `start_listening` turns on the base
/// stream; the opt-in flags are latched by issuing the matching command, and
/// (like the reference) latch even when that command returns an error, because
/// issuing it is what proves the client understands the event.
#[derive(Clone, Copy, Debug, Default)]
pub struct EventSubscriptions {
    pub listening: bool,
    pub thread_diagnostics: bool,
    pub network_topology: bool,
    pub webrtc: bool,
}

impl EventSubscriptions {
    /// Latch the opt-ins implied by a command name.
    pub fn observe_command(&mut self, command: &str) {
        match command {
            "start_listening" => self.listening = true,
            "get_thread_diagnostics" | "get_thread_border_routers" => {
                self.thread_diagnostics = true
            }
            "get_network_topology" => self.network_topology = true,
            "send_webrtc_provider_command" => self.webrtc = true,
            _ => {}
        }
    }

    pub fn accepts(&self, event: &Event) -> bool {
        if !self.listening {
            return false;
        }
        match event.class {
            EventClass::Always => true,
            EventClass::ThreadDiagnostics => self.thread_diagnostics,
            EventClass::NetworkTopology => self.network_topology,
            EventClass::WebRtc => self.webrtc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_updated_is_a_positional_triple() {
        let event = Event::attribute_updated(1, "1/6/0", json!(true));
        assert_eq!(event.payload["event"], json!("attribute_updated"));
        assert_eq!(event.payload["data"], json!([1, "1/6/0", true]));
    }

    #[test]
    fn node_removed_carries_a_bare_node_id() {
        assert_eq!(Event::node_removed(4).payload["data"], json!(4));
    }

    #[test]
    fn nothing_is_delivered_before_start_listening() {
        let subscriptions = EventSubscriptions::default();
        assert!(!subscriptions.accepts(&Event::node_removed(1)));
    }

    #[test]
    fn opt_in_events_need_their_command_first() {
        let mut subscriptions = EventSubscriptions::default();
        subscriptions.observe_command("start_listening");
        let topology = Event::network_topology_updated(&NetworkTopology::default());
        assert!(subscriptions.accepts(&Event::node_removed(1)));
        assert!(!subscriptions.accepts(&topology));

        subscriptions.observe_command("get_network_topology");
        assert!(subscriptions.accepts(&topology));
        assert!(!subscriptions.accepts(&Event::thread_diagnostics_updated(json!({}))));
    }

    #[test]
    fn thread_opt_in_covers_both_thread_commands() {
        let mut subscriptions = EventSubscriptions::default();
        subscriptions.observe_command("start_listening");
        subscriptions.observe_command("get_thread_border_routers");
        assert!(subscriptions.accepts(&Event::thread_diagnostics_updated(json!({}))));
    }
}
