//! Fabric membership, access control, and bindings on a commissioned node.
//!
//! All four commands are ordinary Interaction Model operations against the
//! node's `OperationalCredentials`, `AccessControl` and `Binding` clusters, so
//! they compose from the actor's read/write/invoke rather than needing their
//! own transport support.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::matter::tlv_json::TlvNode;
use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::message::Args;
use crate::protocol::model::{AttributeWritePath, AttributeWriteResult, MatterFabricData};
use crate::protocol::paths::parse_path;

use super::{require_node, CallContext};

const OPERATIONAL_CREDENTIALS_CLUSTER: u32 = 62;
const FABRICS_ATTRIBUTE: u32 = 1;
const UPDATE_FABRIC_LABEL_COMMAND: u32 = 9;
const REMOVE_FABRIC_COMMAND: u32 = 10;

const ACCESS_CONTROL_CLUSTER: u32 = 31;
const ACL_ATTRIBUTE: u32 = 0;

const BINDING_CLUSTER: u32 = 30;
const BINDING_ATTRIBUTE: u32 = 0;

/// `FabricDescriptorStruct` TLV tags.
mod fabric_tags {
    pub const VENDOR_ID: &str = "2";
    pub const FABRIC_ID: &str = "3";
    pub const LABEL: &str = "5";
    pub const FABRIC_INDEX: &str = "254";
}

/// List the fabrics a node belongs to.
pub async fn get_matter_fabrics(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let attributes = context
        .server
        .matter
        .read_attributes(
            node_id,
            vec![parse_path(&format!(
                "0/{}/{}",
                OPERATIONAL_CREDENTIALS_CLUSTER, FABRICS_ATTRIBUTE
            ))?
            .to_attr_path()],
            false,
        )
        .await?;

    let raw = attributes
        .get(&format!(
            "0/{}/{}",
            OPERATIONAL_CREDENTIALS_CLUSTER, FABRICS_ATTRIBUTE
        ))
        .cloned()
        .unwrap_or(Value::Null);

    let fabrics: Vec<MatterFabricData> = raw
        .as_array()
        .map(|entries| entries.iter().map(fabric_from_tlv_json).collect())
        .unwrap_or_default();

    Ok(serde_json::to_value(fabrics).unwrap_or(Value::Null))
}

/// Attribute values arrive tag-based, so struct members are keyed by their TLV
/// tag; the vendor name is resolved from the same table `get_vendor_names` uses.
fn fabric_from_tlv_json(entry: &Value) -> MatterFabricData {
    let vendor_id = entry
        .get(fabric_tags::VENDOR_ID)
        .and_then(Value::as_u64)
        .map(|id| id as u16);
    MatterFabricData {
        fabric_id: entry.get(fabric_tags::FABRIC_ID).and_then(Value::as_u64),
        vendor_id,
        fabric_index: entry
            .get(fabric_tags::FABRIC_INDEX)
            .and_then(Value::as_u64)
            .map(|index| index as u8),
        fabric_label: entry
            .get(fabric_tags::LABEL)
            .and_then(Value::as_str)
            .map(str::to_string),
        vendor_name: vendor_id.and_then(super::vendor_name),
    }
}

pub async fn remove_matter_fabric(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let fabric_index = args.req_u64("fabric_index")?;
    if fabric_index == 0 || fabric_index > u8::MAX as u64 {
        return Err(ApiError::invalid_args("fabric_index must be 1..=255"));
    }

    context
        .server
        .matter
        .invoke(
            node_id,
            0,
            OPERATIONAL_CREDENTIALS_CLUSTER,
            REMOVE_FABRIC_COMMAND,
            TlvNode::Struct(vec![(0, TlvNode::U64(fabric_index))]),
            None,
            BTreeMap::new(),
        )
        .await?;
    Ok(json!({}))
}

/// `NOCResponse.StatusCode`. The invoke below passes no response names, so the
/// reply decodes tag-based and the status arrives under its numeric tag.
const NOC_RESPONSE_STATUS_TAG: &str = "0";
/// `NodeOperationalCertStatusEnum`, the two values that matter here.
const NOC_STATUS_OK: u64 = 0;
const NOC_STATUS_LABEL_CONFLICT: u64 = 10;

/// Tell a node what to call this controller's fabric.
///
/// The label is what other ecosystems show a user when they list the fabrics a
/// device belongs to, so a device that was never told stays blank there. It is
/// pushed after commissioning and whenever the configured label changes.
///
/// The device answers with an `NOCResponse`, and its status is the whole point
/// of the call. Labels must be unique across the fabrics on one device, so a
/// second controller that defaults to the same name as the first is refused
/// with `LabelConflict` — which is not a rare case but the ordinary one, since
/// every installation of this server defaults to the same label. Returning
/// `Ok` on a refusal left the fabric nameless on the device and said nothing.
pub async fn push_fabric_label(
    context: CallContext<'_>,
    node_id: u64,
    label: &str,
) -> Result<(), ApiError> {
    let response = context
        .server
        .matter
        .invoke(
            node_id,
            0,
            OPERATIONAL_CREDENTIALS_CLUSTER,
            UPDATE_FABRIC_LABEL_COMMAND,
            TlvNode::Struct(vec![(0, TlvNode::Utf8(label.to_string()))]),
            None,
            BTreeMap::new(),
        )
        .await?;

    noc_response_result(&response, label)
}

/// Read the status out of an `NOCResponse` and say whether the device agreed.
///
/// Separate from the invoke so the decision can be tested: the invoke needs a
/// device, and this is the part that was wrong.
fn noc_response_result(response: &Value, label: &str) -> Result<(), ApiError> {
    // A response with no status is not an error: a device that answers the
    // invoke without the field has still accepted the command.
    let status = response
        .get(NOC_RESPONSE_STATUS_TAG)
        .and_then(Value::as_u64)
        .unwrap_or(NOC_STATUS_OK);

    if status == NOC_STATUS_OK {
        return Ok(());
    }
    Err(ApiError::sdk(match status {
        NOC_STATUS_LABEL_CONFLICT => format!(
            "the device already has a fabric labelled '{}'; labels must differ across the fabrics on one device",
            label
        ),
        other => format!("the device refused the fabric label with status {}", other),
    }))
}

/// Replace this fabric's ACL entries on a node.
///
/// The node's own fabric index is filled in by the device, so entries are sent
/// without one — writing a fabric-scoped list only ever touches the writer's
/// own fabric.
pub async fn set_acl_entry(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let entries = args.req_array("entry")?;

    let mut encoded = Vec::with_capacity(entries.len());
    for entry in entries {
        encoded.push(acl_entry_to_tlv(entry)?);
    }

    let status = context
        .server
        .matter
        .write_attribute(
            node_id,
            0,
            ACCESS_CONTROL_CLUSTER,
            ACL_ATTRIBUTE,
            TlvNode::Array(encoded),
            None,
        )
        .await?;

    Ok(serde_json::to_value(vec![AttributeWriteResult {
        path: AttributeWritePath {
            endpoint_id: 0,
            cluster_id: ACCESS_CONTROL_CLUSTER,
            attribute_id: ACL_ATTRIBUTE,
        },
        status,
    }])
    .unwrap_or(Value::Null))
}

/// `AccessControlEntryStruct`: privilege 1, authMode 2, subjects 3, targets 4.
fn acl_entry_to_tlv(entry: &Value) -> Result<TlvNode, ApiError> {
    let privilege = entry
        .get("privilege")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::invalid_args("Each ACL entry needs a privilege"))?;
    let auth_mode = entry
        .get("auth_mode")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::invalid_args("Each ACL entry needs an auth_mode"))?;

    let subjects = match entry.get("subjects") {
        None | Some(Value::Null) => TlvNode::Null,
        Some(Value::Array(items)) => TlvNode::Array(
            items
                .iter()
                .map(|item| {
                    item.as_u64()
                        .map(TlvNode::U64)
                        .ok_or_else(|| ApiError::invalid_args("ACL subjects must be integers"))
                })
                .collect::<Result<_, _>>()?,
        ),
        Some(_) => return Err(ApiError::invalid_args("ACL subjects must be an array")),
    };

    let targets = match entry.get("targets") {
        None | Some(Value::Null) => TlvNode::Null,
        Some(Value::Array(items)) => TlvNode::Array(
            items
                .iter()
                .map(acl_target_to_tlv)
                .collect::<Result<_, _>>()?,
        ),
        Some(_) => return Err(ApiError::invalid_args("ACL targets must be an array")),
    };

    Ok(TlvNode::Struct(vec![
        (1, TlvNode::U64(privilege)),
        (2, TlvNode::U64(auth_mode)),
        (3, subjects),
        (4, targets),
    ]))
}

/// `AccessControlTargetStruct`: cluster 0, endpoint 1, deviceType 2. Absent
/// members are sent as null, which is how the struct spells "unrestricted".
fn acl_target_to_tlv(target: &Value) -> Result<TlvNode, ApiError> {
    let member = |name: &str| match target.get(name) {
        None | Some(Value::Null) => Ok(TlvNode::Null),
        Some(Value::Number(number)) => number.as_u64().map(TlvNode::U64).ok_or_else(|| {
            ApiError::invalid_args(format!("ACL target {} must be an integer", name))
        }),
        Some(_) => Err(ApiError::invalid_args(format!(
            "ACL target {} must be an integer or null",
            name
        ))),
    };
    Ok(TlvNode::Struct(vec![
        (0, member("cluster")?),
        (1, member("endpoint")?),
        (2, member("device_type")?),
    ]))
}

/// Replace the binding list on one endpoint.
pub async fn set_node_binding(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let endpoint = args.req_u16("endpoint")?;
    let bindings = args.req_array("bindings")?;

    let mut encoded = Vec::with_capacity(bindings.len());
    for binding in bindings {
        encoded.push(binding_to_tlv(binding)?);
    }

    let status = context
        .server
        .matter
        .write_attribute(
            node_id,
            endpoint,
            BINDING_CLUSTER,
            BINDING_ATTRIBUTE,
            TlvNode::Array(encoded),
            None,
        )
        .await?;

    Ok(serde_json::to_value(vec![AttributeWriteResult {
        path: AttributeWritePath {
            endpoint_id: endpoint,
            cluster_id: BINDING_CLUSTER,
            attribute_id: BINDING_ATTRIBUTE,
        },
        status,
    }])
    .unwrap_or(Value::Null))
}

/// Binding `TargetStruct`: node 1, group 2, endpoint 3, cluster 4. A target
/// names either a node (with an endpoint) or a group, never both, and members
/// that are absent are omitted rather than nulled.
fn binding_to_tlv(binding: &Value) -> Result<TlvNode, ApiError> {
    let mut members = Vec::new();
    let mut push = |tag: u8, name: &str| -> Result<bool, ApiError> {
        match binding.get(name) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Number(number)) => {
                let value = number.as_u64().ok_or_else(|| {
                    ApiError::invalid_args(format!("Binding {} must be an integer", name))
                })?;
                members.push((tag, TlvNode::U64(value)));
                Ok(true)
            }
            Some(_) => Err(ApiError::invalid_args(format!(
                "Binding {} must be an integer",
                name
            ))),
        }
    };

    let has_node = push(1, "node")?;
    let has_group = push(2, "group")?;
    push(3, "endpoint")?;
    push(4, "cluster")?;

    if has_node == has_group {
        return Err(ApiError::invalid_args(
            "Each binding must target either a node or a group",
        ));
    }
    members.sort_by_key(|(tag, _)| *tag);
    Ok(TlvNode::Struct(members))
}

#[cfg(test)]
mod tests {
    /// Regression: two controllers on one device, both defaulting to the same
    /// label. The device answers `LabelConflict` (10) and the label never
    /// lands; returning `Ok` on that made a nameless fabric look like a
    /// success. Found with a real plug commissioned onto two fabrics.
    #[test]
    fn a_refused_fabric_label_is_not_reported_as_success() {
        // The tag-based shape `push_fabric_label` receives, since it invokes
        // with no response names. This is the real plug's answer when a
        // second controller asks for a label the first already has.
        let refused = json!({ NOC_RESPONSE_STATUS_TAG: NOC_STATUS_LABEL_CONFLICT });
        let error = noc_response_result(&refused, "Home").unwrap_err();
        assert!(
            error
                .details
                .contains("already has a fabric labelled 'Home'"),
            "{}",
            error.details
        );

        // An accepted label, and a device answering without the field at all,
        // are both success.
        let ok = json!({ NOC_RESPONSE_STATUS_TAG: NOC_STATUS_OK });
        assert!(noc_response_result(&ok, "x").is_ok());
        assert!(noc_response_result(&json!({}), "x").is_ok());

        // Any other refusal is reported rather than swallowed.
        let other = noc_response_result(&json!({ NOC_RESPONSE_STATUS_TAG: 11 }), "x").unwrap_err();
        assert!(other.details.contains("status 11"), "{}", other.details);
    }

    use super::*;
    use crate::api::tests_support::{call, test_context};
    use crate::protocol::model::MatterNodeData;
    use crate::storage::StoredNode;
    use futures_lite::future::block_on;

    fn context_with_node() -> crate::api::ServerContext {
        let context = test_context();
        context.nodes.upsert(StoredNode::new(MatterNodeData::new(
            1,
            "2026-01-01T00:00:00.000Z".into(),
        )));
        context
    }

    #[test]
    fn fabric_descriptors_decode_from_tag_keyed_structs() {
        let entry = json!({
            "1": "cHVibGljLWtleQ==",
            "2": 4874,
            "3": 1234567890,
            "4": 112233,
            "5": "My Home",
            "254": 2
        });
        let fabric = fabric_from_tlv_json(&entry);
        assert_eq!(fabric.fabric_id, Some(1234567890));
        assert_eq!(fabric.vendor_id, Some(4874));
        assert_eq!(fabric.fabric_index, Some(2));
        assert_eq!(fabric.fabric_label.as_deref(), Some("My Home"));
        assert_eq!(fabric.vendor_name.as_deref(), Some("EVE SYSTEMS"));
    }

    #[test]
    fn acl_entries_encode_to_the_spec_tags() {
        let entry = json!({
            "privilege": 5,
            "auth_mode": 2,
            "subjects": [112233],
            "targets": null
        });
        assert_eq!(
            acl_entry_to_tlv(&entry).unwrap(),
            TlvNode::Struct(vec![
                (1, TlvNode::U64(5)),
                (2, TlvNode::U64(2)),
                (3, TlvNode::Array(vec![TlvNode::U64(112233)])),
                (4, TlvNode::Null),
            ])
        );
    }

    #[test]
    fn acl_targets_null_their_unrestricted_members() {
        let target = json!({ "cluster": 6, "endpoint": null, "device_type": null });
        assert_eq!(
            acl_target_to_tlv(&target).unwrap(),
            TlvNode::Struct(vec![
                (0, TlvNode::U64(6)),
                (1, TlvNode::Null),
                (2, TlvNode::Null),
            ])
        );
    }

    #[test]
    fn acl_entries_need_a_privilege_and_auth_mode() {
        assert!(acl_entry_to_tlv(&json!({ "auth_mode": 2 }))
            .unwrap_err()
            .details
            .contains("privilege"));
        assert!(acl_entry_to_tlv(&json!({ "privilege": 5 }))
            .unwrap_err()
            .details
            .contains("auth_mode"));
    }

    #[test]
    fn bindings_target_either_a_node_or_a_group() {
        let node_binding = json!({ "node": 2, "endpoint": 1, "cluster": 6 });
        assert_eq!(
            binding_to_tlv(&node_binding).unwrap(),
            TlvNode::Struct(vec![
                (1, TlvNode::U64(2)),
                (3, TlvNode::U64(1)),
                (4, TlvNode::U64(6)),
            ])
        );

        let neither = json!({ "endpoint": 1, "cluster": 6 });
        assert!(binding_to_tlv(&neither).is_err());
        let both = json!({ "node": 2, "group": 3 });
        assert!(binding_to_tlv(&both).is_err());
    }

    #[test]
    fn fabric_index_zero_is_rejected() {
        let context = context_with_node();
        let args = Args::new(json!({ "node_id": 1, "fabric_index": 0 }));
        let error = block_on(remove_matter_fabric(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
    }

    #[test]
    fn acl_and_binding_writes_need_their_lists() {
        let context = context_with_node();
        let args = Args::new(json!({ "node_id": 1 }));
        assert!(block_on(set_acl_entry(&args, call(&context)))
            .unwrap_err()
            .details
            .contains("Missing entry"));
        let args = Args::new(json!({ "node_id": 1, "endpoint": 1 }));
        assert!(block_on(set_node_binding(&args, call(&context)))
            .unwrap_err()
            .details
            .contains("Missing bindings"));
    }
}
