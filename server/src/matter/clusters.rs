//! Cluster metadata registry.
//!
//! `device_command` addresses commands and their payload fields by *name*, so
//! the server needs the same cluster metadata the reference gets from
//! matter.js. The table is lifted from rs-matter's own generated cluster code
//! (see `clusters.json`) and the names are converted with the reference's wire
//! naming rule, so `command_name: "moveToLevelWithOnOff"` and a payload of
//! `{"level": 128}` resolve to the same ids on both servers.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

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
    /// Struct definitions the commands above refer to by name.
    #[serde(default)]
    structs: BTreeMap<String, RawStruct>,
    /// Attribute id (as a string key) -> `us` or `s`, for the attributes the
    /// Matter IDL types as an epoch.
    #[serde(default)]
    epoch: BTreeMap<String, String>,
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

/// A struct a payload field can hold, named by the cluster that defines it.
#[derive(Debug, Deserialize)]
struct RawStruct {
    fields: BTreeMap<String, u32>,
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
    epoch: BTreeMap<u32, EpochUnit>,
}

/// How an epoch-typed attribute counts.
///
/// Matter measures both from 2000-01-01T00:00:00 UTC; the reference reports
/// them as Unix time, so the conversion happens at the codec boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochUnit {
    Seconds,
    Micros,
}

impl EpochUnit {
    /// How far the Matter epoch sits after the Unix one, in this unit.
    pub const fn unix_offset(self) -> u64 {
        const MATTER_EPOCH_UNIX_SECS: u64 = 946_684_800;
        match self {
            Self::Seconds => MATTER_EPOCH_UNIX_SECS,
            Self::Micros => MATTER_EPOCH_UNIX_SECS * 1_000_000,
        }
    }
}

/// What a payload field holds. TLV is self-describing on read, so this is
/// only consulted when encoding: it is what tells a JSON string destined for
/// an octet-string field to be decoded from base64 rather than sent as text,
/// and a JSON object destined for a struct field which of its keys are names.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    Bytes,
    String,
    Int,
    Float,
    Bool,
    /// A list, carrying what its elements are.
    List(Box<FieldKind>),
    /// A struct whose fields have names of their own.
    Struct(Arc<StructMeta>),
    /// An enum, bitmap, or anything else that encodes from the JSON shape
    /// alone.
    Other,
}

impl FieldKind {
    /// Resolve one generated kind string against the cluster's struct table.
    ///
    /// `structs` is being built as this runs, so a definition that is still
    /// on the stack — only possible if rs-matter ever emits a cyclic struct —
    /// resolves to `Other` rather than recursing forever.
    fn parse(raw: &str, definitions: &BTreeMap<String, RawStruct>, resolver: &mut Resolver) -> Self {
        if let Some(element) = raw.strip_prefix("list:") {
            return Self::List(Box::new(Self::parse(element, definitions, resolver)));
        }
        if let Some(name) = raw.strip_prefix("struct:") {
            return match resolver.resolve(name, definitions) {
                Some(meta) => Self::Struct(meta),
                None => Self::Other,
            };
        }
        match raw {
            "bytes" => Self::Bytes,
            "string" => Self::String,
            "int" => Self::Int,
            "float" => Self::Float,
            "bool" => Self::Bool,
            _ => Self::Other,
        }
    }

    /// What a list's elements hold. A field that is not a list has no
    /// elements, so its own kind is the best answer for the values inside it.
    pub fn element(&self) -> &FieldKind {
        match self {
            Self::List(element) => element,
            other => other,
        }
    }
}

/// A struct nested inside a payload, with its own named fields.
#[derive(Debug, PartialEq)]
pub struct StructMeta {
    /// Wire name, e.g. `credentialStruct`.
    pub name: String,
    /// Field wire name -> TLV context tag.
    fields: Vec<(String, u32)>,
    /// TLV context tag -> field kind.
    kinds: BTreeMap<u32, FieldKind>,
}

impl StructMeta {
    /// Look up the TLV tag for a field name, as [`CommandMeta::request_tag`]
    /// does one level up.
    pub fn tag(&self, field: &str) -> Option<u32> {
        let wanted = normalize(field);
        self.fields
            .iter()
            .find(|(name, _)| normalize(name) == wanted)
            .map(|(_, tag)| *tag)
    }

    pub fn kind(&self, tag: u32) -> &FieldKind {
        self.kinds.get(&tag).unwrap_or(&FieldKind::Other)
    }
}

/// Turns the generated struct table into resolved [`StructMeta`] values,
/// sharing one instance between every field that names it.
#[derive(Default)]
struct Resolver {
    done: BTreeMap<String, Arc<StructMeta>>,
    /// Definitions currently being resolved, so a cycle can be broken.
    active: Vec<String>,
}

impl Resolver {
    fn resolve(
        &mut self,
        name: &str,
        definitions: &BTreeMap<String, RawStruct>,
    ) -> Option<Arc<StructMeta>> {
        if let Some(meta) = self.done.get(name) {
            return Some(Arc::clone(meta));
        }
        if self.active.iter().any(|active| active == name) {
            return None;
        }
        let definition = definitions.get(name)?;
        self.active.push(name.to_string());
        let kinds = definition
            .types
            .iter()
            .filter_map(|(tag, kind)| Some((tag.parse().ok()?, FieldKind::parse(kind, definitions, self))))
            .collect();
        self.active.pop();

        let meta = Arc::new(StructMeta {
            name: wire_field_name(name),
            fields: definition
                .fields
                .iter()
                .map(|(field, tag)| (wire_field_name(field), *tag))
                .collect(),
            kinds,
        });
        self.done.insert(name.to_string(), Arc::clone(&meta));
        Some(meta)
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
    request_kinds: BTreeMap<u32, FieldKind>,
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

    /// The epoch unit an attribute is typed with, if any.
    pub fn attribute_epoch(&self, id: u32) -> Option<EpochUnit> {
        self.epoch.get(&id).copied()
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

    pub fn request_kind(&self, tag: u32) -> &FieldKind {
        self.request_kinds.get(&tag).unwrap_or(&FieldKind::Other)
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
                // One resolver per cluster: struct definitions are
                // cluster-scoped, and every field naming the same struct
                // should share the one instance.
                let definitions = cluster.structs;
                let mut resolver = Resolver::default();
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
                            .iter()
                            .filter_map(|(tag, kind)| {
                                Some((
                                    tag.parse().ok()?,
                                    FieldKind::parse(kind, &definitions, &mut resolver),
                                ))
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
                        epoch: cluster
                            .epoch
                            .into_iter()
                            .filter_map(|(id, unit)| {
                                let unit = match unit.as_str() {
                                    "us" => EpochUnit::Micros,
                                    "s" => EpochUnit::Seconds,
                                    _ => return None,
                                };
                                Some((id.parse().ok()?, unit))
                            })
                            .collect(),
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
        assert_eq!(*command.request_kind(dataset_tag), FieldKind::Bytes);
        let breadcrumb_tag = command.request_tag("breadcrumb").unwrap();
        assert_eq!(*command.request_kind(breadcrumb_tag), FieldKind::Int);
    }

    #[test]
    fn a_struct_typed_field_carries_its_own_schema() {
        let door_lock = cluster(257).unwrap();
        let command = door_lock.command("setCredential").unwrap();
        let tag = command.request_tag("credential").unwrap();
        let FieldKind::Struct(credential) = command.request_kind(tag) else {
            panic!("credential should be a struct, got {:?}", command.request_kind(tag));
        };
        assert_eq!(credential.name, "credentialStruct");
        assert_eq!(credential.tag("credentialType"), Some(0));
        assert_eq!(credential.tag("credentialIndex"), Some(1));
        assert_eq!(*credential.kind(1), FieldKind::Int);
        assert_eq!(credential.tag("nonexistent"), None);
    }

    #[test]
    fn a_list_field_carries_the_kind_of_its_elements() {
        let content_control = cluster(1295).unwrap();
        let command = content_control.command("addBlockApplications").unwrap();
        let tag = command.request_tag("applications").unwrap();
        let kind = command.request_kind(tag);
        assert!(matches!(kind, FieldKind::List(_)), "got {:?}", kind);
        let FieldKind::Struct(app) = kind.element() else {
            panic!("elements should be structs, got {:?}", kind.element());
        };
        assert_eq!(app.tag("catalogVendorID"), Some(0));
    }

    /// The same struct reached from two commands is one instance, so the
    /// table cannot grow a copy per reference.
    #[test]
    fn struct_definitions_are_shared_between_commands() {
        let application_launcher = cluster(1292).unwrap();
        let launch = application_launcher.command("launchApp").unwrap();
        let stop = application_launcher.command("stopApp").unwrap();
        let (FieldKind::Struct(from_launch), FieldKind::Struct(from_stop)) = (
            launch.request_kind(launch.request_tag("application").unwrap()),
            stop.request_kind(stop.request_tag("application").unwrap()),
        ) else {
            panic!("application should be a struct on both commands");
        };
        assert!(Arc::ptr_eq(from_launch, from_stop));
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
    fn epoch_typed_attributes_are_labelled_with_their_unit() {
        let time_sync = cluster(56).unwrap();
        let utc_time = time_sync.attribute_id("UTCTime").unwrap();
        assert_eq!(time_sync.attribute_epoch(utc_time), Some(EpochUnit::Micros));

        let evse = cluster(153).unwrap();
        let next_start = evse.attribute_id("NextChargeStartTime").unwrap();
        assert_eq!(evse.attribute_epoch(next_start), Some(EpochUnit::Seconds));

        // An ordinary integer attribute is not an epoch, and neither is an
        // attribute id that belongs to a different cluster.
        assert_eq!(cluster(6).unwrap().attribute_epoch(0), None);
        assert_eq!(time_sync.attribute_epoch(1), None);
    }

    #[test]
    fn the_matter_epoch_offset_is_the_2000_01_01_boundary() {
        // 1970-01-01 to 2000-01-01 is 30 years with 7 leap days.
        assert_eq!(EpochUnit::Seconds.unix_offset(), 946_684_800);
        assert_eq!(EpochUnit::Micros.unix_offset(), 946_684_800_000_000);
    }

    #[test]
    fn attributes_resolve_in_both_directions() {
        let basic = cluster(40).unwrap();
        assert_eq!(basic.attribute_id("VendorName"), Some(1));
        assert_eq!(basic.attribute_name(5), Some("NodeLabel"));
    }
}
