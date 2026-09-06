//! Cluster metadata registry.
//!
//! `device_command` addresses commands and their payload fields by *name*, so
//! the server needs the same cluster metadata the reference gets from
//! matter.js. The table is lifted from rs-matter's own generated cluster code
//! (see `clusters.json`) and the names are converted with the reference's wire
//! naming rule, so `command_name: "moveToLevelWithOnOff"` and a payload of
//! `{"level": 128}` resolve to the same ids on both servers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use super::wire_naming::wire_field_name;

/// Generated from rs-matter's cluster definitions; see the module docs.
const REGISTRY_JSON: &str = include_str!("clusters.json");

#[derive(Debug, Deserialize)]
struct RawCluster {
    name: String,
    #[allow(dead_code)]
    revision: u32,
    #[serde(default)]
    commands: BTreeMap<String, RawCommand>,
    #[serde(default)]
    attributes: BTreeMap<String, u32>,
    #[serde(default)]
    events: BTreeMap<String, u32>,
}

#[derive(Debug, Deserialize)]
struct RawCommand {
    id: u32,
    #[serde(default)]
    req: BTreeMap<String, u32>,
    #[serde(default)]
    resp: BTreeMap<String, u32>,
    /// TLV tag (as a string key) -> field kind, used when encoding a payload.
    #[serde(default)]
    types: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct ClusterMeta {
    pub id: u32,
    pub name: String,
    commands: Vec<CommandMeta>,
    attributes: Vec<(String, u32)>,
    events: Vec<(String, u32)>,
}

/// What a payload field holds. TLV is self-describing on read, so this is
/// only consulted when encoding: it is what tells a JSON string destined for
/// an octet-string field to be decoded from base64 rather than sent as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Bytes,
    String,
    Int,
    Float,
    Bool,
    List,
    /// A struct, enum, bitmap, or nullable wrapper: encoded from the JSON
    /// shape alone.
    Other,
}

impl FieldKind {
    fn parse(raw: &str) -> Self {
        match raw {
            "bytes" => Self::Bytes,
            "string" => Self::String,
            "int" => Self::Int,
            "float" => Self::Float,
            "bool" => Self::Bool,
            "list" => Self::List,
            _ => Self::Other,
        }
    }
}

#[derive(Debug)]
pub struct CommandMeta {
    pub id: u32,
    /// Wire name, e.g. `moveToLevelWithOnOff`.
    pub name: String,
    /// Payload field wire name -> TLV context tag.
    pub request_fields: Vec<(String, u32)>,
    /// TLV context tag -> response field wire name.
    pub response_fields: BTreeMap<u32, String>,
    /// TLV context tag -> payload field kind.
    pub request_kinds: BTreeMap<u32, FieldKind>,
}

impl ClusterMeta {
    /// Resolve a command by the name a client sent, or by its numeric id.
    ///
    /// Matching ignores case and separators, so `moveToLevelWithOnOff`,
    /// `MoveToLevelWithOnOff` and `move_to_level_with_on_off` all resolve —
    /// clients generated from different SDKs spell them differently.
    pub fn command(&self, name: &str) -> Option<&CommandMeta> {
        let wanted = normalize(name);
        self.commands
            .iter()
            .find(|command| normalize(&command.name) == wanted)
    }

    pub fn command_by_id(&self, id: u32) -> Option<&CommandMeta> {
        self.commands.iter().find(|command| command.id == id)
    }

    pub fn attribute_id(&self, name: &str) -> Option<u32> {
        let wanted = normalize(name);
        self.attributes
            .iter()
            .find(|(attribute, _)| normalize(attribute) == wanted)
            .map(|(_, id)| *id)
    }

    pub fn attribute_name(&self, id: u32) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(_, attribute_id)| *attribute_id == id)
            .map(|(name, _)| name.as_str())
    }

    pub fn event_name(&self, id: u32) -> Option<&str> {
        self.events
            .iter()
            .find(|(_, event_id)| *event_id == id)
            .map(|(name, _)| name.as_str())
    }
}

impl CommandMeta {
    /// Look up the TLV tag for a payload field name.
    pub fn request_tag(&self, field: &str) -> Option<u32> {
        let wanted = normalize(field);
        self.request_fields
            .iter()
            .find(|(name, _)| normalize(name) == wanted)
            .map(|(_, tag)| *tag)
    }

    pub fn has_response_fields(&self) -> bool {
        !self.response_fields.is_empty()
    }

    pub fn request_kind(&self, tag: u32) -> FieldKind {
        self.request_kinds
            .get(&tag)
            .copied()
            .unwrap_or(FieldKind::Other)
    }
}

/// Case- and separator-insensitive key for name matching.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn registry() -> &'static BTreeMap<u32, ClusterMeta> {
    static REGISTRY: OnceLock<BTreeMap<u32, ClusterMeta>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let raw: BTreeMap<String, RawCluster> =
            serde_json::from_str(REGISTRY_JSON).expect("bundled cluster registry is valid JSON");
        raw.into_iter()
            .filter_map(|(cluster_id, cluster)| {
                let cluster_id = cluster_id.parse::<u32>().ok()?;
                let commands = cluster
                    .commands
                    .into_iter()
                    .map(|(name, command)| CommandMeta {
                        id: command.id,
                        name: wire_field_name(&name),
                        request_fields: command
                            .req
                            .into_iter()
                            .map(|(field, tag)| (wire_field_name(&field), tag))
                            .collect(),
                        response_fields: command
                            .resp
                            .into_iter()
                            .map(|(field, tag)| (tag, wire_field_name(&field)))
                            .collect(),
                        request_kinds: command
                            .types
                            .into_iter()
                            .filter_map(|(tag, kind)| {
                                Some((tag.parse().ok()?, FieldKind::parse(&kind)))
                            })
                            .collect(),
                    })
                    .collect();
                Some((
                    cluster_id,
                    ClusterMeta {
                        id: cluster_id,
                        name: cluster.name,
                        commands,
                        attributes: cluster.attributes.into_iter().collect(),
                        events: cluster.events.into_iter().collect(),
                    },
                ))
            })
            .collect()
    })
}

pub fn cluster(cluster_id: u32) -> Option<&'static ClusterMeta> {
    registry().get(&cluster_id)
}

pub fn cluster_count() -> usize {
    registry().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_every_cluster() {
        assert!(cluster_count() > 100, "got {}", cluster_count());
    }

    #[test]
    fn resolves_on_off_commands_by_wire_name() {
        let on_off = cluster(6).unwrap();
        assert_eq!(on_off.name, "OnOff");
        assert_eq!(on_off.command("on").unwrap().id, 1);
        assert_eq!(on_off.command("off").unwrap().id, 0);
        assert_eq!(on_off.command("toggle").unwrap().id, 2);
        // Clients from other SDKs spell it differently.
        assert_eq!(on_off.command("Toggle").unwrap().id, 2);
    }

    #[test]
    fn resolves_level_control_payload_fields() {
        let level = cluster(8).unwrap();
        let command = level.command("moveToLevelWithOnOff").unwrap();
        assert_eq!(command.id, 4);
        assert_eq!(command.request_tag("level"), Some(0));
        assert_eq!(command.request_tag("transitionTime"), Some(1));
        assert_eq!(command.request_tag("optionsMask"), Some(2));
        assert_eq!(command.request_tag("nonexistent"), None);
    }

    #[test]
    fn response_fields_are_indexed_by_tag() {
        let general_commissioning = cluster(48).unwrap();
        let command = general_commissioning.command("armFailSafe").unwrap();
        assert!(command.has_response_fields());
        assert_eq!(command.response_fields.get(&0).unwrap(), "errorCode");
        assert_eq!(command.response_fields.get(&1).unwrap(), "debugText");
    }

    #[test]
    fn payload_field_kinds_are_available_for_encoding() {
        let network = cluster(49).unwrap();
        let command = network.command("addOrUpdateThreadNetwork").unwrap();
        let dataset_tag = command.request_tag("operationalDataset").unwrap();
        assert_eq!(command.request_kind(dataset_tag), FieldKind::Bytes);
        let breadcrumb_tag = command.request_tag("breadcrumb").unwrap();
        assert_eq!(command.request_kind(breadcrumb_tag), FieldKind::Int);
    }

    #[test]
    fn door_lock_commands_carry_their_payload_schema() {
        let door_lock = cluster(257).unwrap();
        let lock = door_lock.command("lockDoor").unwrap();
        assert_eq!(lock.id, 0);
        // Clients send this as PINCode or pinCode depending on their SDK.
        assert_eq!(lock.request_tag("PINCode"), lock.request_tag("pinCode"));
        assert!(lock.request_tag("PINCode").is_some());
    }

    #[test]
    fn attributes_resolve_in_both_directions() {
        let basic = cluster(40).unwrap();
        assert_eq!(basic.attribute_id("VendorName"), Some(1));
        assert_eq!(basic.attribute_name(5), Some("NodeLabel"));
    }
}
