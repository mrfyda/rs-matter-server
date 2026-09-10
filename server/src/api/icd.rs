//! Intermittently Connected Device (ICD) check-in registration.
//!
//! A long-idle-time (LIT) device sleeps until it checks in with a registered
//! client. Registering this controller as one is what keeps such a device
//! reachable; the commands here read and change that registration on the peer.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::matter::tlv_json::TlvNode;
use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::message::Args;
use crate::protocol::model::{IcdOperatingMode, IcdStateData};
use crate::protocol::paths::parse_path;

use super::{require_node, CallContext};

const ICD_MANAGEMENT_CLUSTER: u32 = 70;
const IDLE_MODE_DURATION_ATTRIBUTE: u32 = 0;
const ACTIVE_MODE_DURATION_ATTRIBUTE: u32 = 1;
const REGISTERED_CLIENTS_ATTRIBUTE: u32 = 3;
const OPERATING_MODE_ATTRIBUTE: u32 = 8;
const FEATURE_MAP_ATTRIBUTE: u32 = 65532;

const REGISTER_CLIENT_COMMAND: u32 = 0;
const UNREGISTER_CLIENT_COMMAND: u32 = 2;

/// `LongIdleTimeSupport` in the ICD Management feature map.
const FEATURE_LONG_IDLE_TIME: u64 = 0x4;

/// `MonitoringRegistrationStruct`: the check-in node id is TLV tag 1.
const CHECK_IN_NODE_ID_TAG: &str = "1";

/// The shared key a check-in registration is authenticated with.
const ICD_KEY_LEN: usize = 16;

const OPERATIONAL_CREDENTIALS_CLUSTER: u32 = 62;
const FABRICS_ATTRIBUTE: u32 = 1;
/// `FabricDescriptorStruct` vendor id tag.
const FABRIC_VENDOR_ID_TAG: &str = "2";

pub async fn get_icd_state(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let state = read_icd_state(node_id, context).await?;
    Ok(serde_json::to_value(state).unwrap_or(Value::Null))
}

async fn read_icd_state(node_id: u64, context: CallContext<'_>) -> Result<IcdStateData, ApiError> {
    let paths = vec![parse_path(&format!("0/{}/*", ICD_MANAGEMENT_CLUSTER))?.to_attr_path()];

    // A node with no ICD Management cluster answers with a status rather than
    // data; that is "not supported", not a failure.
    let Ok(attributes) = context
        .server
        .matter
        .read_attributes(node_id, paths, false)
        .await
    else {
        return Ok(IcdStateData::unsupported());
    };
    if attributes.is_empty() {
        return Ok(IcdStateData::unsupported());
    }

    let attribute = |id: u32| attributes.get(&format!("0/{}/{}", ICD_MANAGEMENT_CLUSTER, id));

    let feature_map = attribute(FEATURE_MAP_ATTRIBUTE)
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let operating_mode = match attribute(OPERATING_MODE_ATTRIBUTE).and_then(Value::as_u64) {
        Some(0) => Some(IcdOperatingMode::Sit),
        Some(1) => Some(IcdOperatingMode::Lit),
        _ => None,
    };

    let controller_node_id = context.server.fabric_info().await.map(|f| f.node_id).ok();
    let registered = attribute(REGISTERED_CLIENTS_ATTRIBUTE)
        .and_then(Value::as_array)
        .map(|clients| {
            clients.iter().any(|client| {
                client.get(CHECK_IN_NODE_ID_TAG).and_then(Value::as_u64) == controller_node_id
            })
        })
        .unwrap_or(false);

    let available = context.server.nodes.get(node_id).map(|node| node.available);

    // `IdleModeDuration` is seconds, `ActiveModeDuration` milliseconds — the
    // cluster mixes units, and getting it wrong would put the next check-in a
    // thousand times too far away.
    let idle_secs = attribute(IDLE_MODE_DURATION_ATTRIBUTE).and_then(Value::as_u64);
    let active_ms = attribute(ACTIVE_MODE_DURATION_ATTRIBUTE).and_then(Value::as_u64);
    let last_check_in = context.server.last_check_in(node_id);

    Ok(IcdStateData {
        supported: true,
        lit_supported: feature_map & FEATURE_LONG_IDLE_TIME != 0,
        registered,
        operating_mode,
        awake: awake(last_check_in, active_ms),
        available,
        next_expected_checkin: next_expected_checkin(last_check_in, idle_secs),
    })
}

/// Whether the device is still inside the window it stays awake for after
/// checking in.
///
/// `None` when it has not checked in since this server started, or when the
/// device does not say how long it stays awake: guessing would be worse than
/// admitting the answer is not known.
fn awake(last_check_in: Option<SystemTime>, active_ms: Option<u64>) -> Option<bool> {
    let elapsed = last_check_in?.elapsed().ok()?;
    Some(elapsed < Duration::from_millis(active_ms?))
}

/// When the device is next due to check in: one idle period after the last
/// one, in epoch milliseconds.
fn next_expected_checkin(last_check_in: Option<SystemTime>, idle_secs: Option<u64>) -> Option<u64> {
    let next = last_check_in? + Duration::from_secs(idle_secs?);
    next.duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_millis() as u64)
}

/// Register this controller as a check-in client.
///
/// A peer that already has administrators from other vendors is rejected
/// unless the caller opts in: those ecosystems may not support LIT, and
/// switching the device into it can make it unreachable for them.
pub async fn register_icd(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let allow_multi_admin = args.bool_or("allow_multi_admin", false)?;
    let ignored_vendors = args.u64_array("ignored_vendors")?.unwrap_or_default();

    if !allow_multi_admin {
        let others = other_vendor_admins(node_id, &ignored_vendors, context).await?;
        if !others.is_empty() {
            return Err(ApiError::icd_multi_admin(&others));
        }
    }

    let fabric = context.server.fabric_info().await?;
    let mut key = vec![0u8; ICD_KEY_LEN];
    getrandom_key(&mut key);

    context
        .server
        .matter
        .invoke(
            node_id,
            0,
            ICD_MANAGEMENT_CLUSTER,
            REGISTER_CLIENT_COMMAND,
            TlvNode::Struct(vec![
                (0, TlvNode::U64(fabric.node_id)),
                (1, TlvNode::U64(fabric.node_id)),
                (2, TlvNode::Bytes(key.clone())),
            ]),
            None,
            BTreeMap::new(),
        )
        .await?;

    // Kept because a check-in carries no readable sender: this key is both the
    // only way to read one from this device and the only way to know it was
    // this device that sent it. Stored only after the device accepted it.
    if let Err(error) = context.server.config.set_icd_registration(node_id, &key) {
        log::warn!(
            "Node {} was registered but its check-in key could not be stored: {}",
            node_id,
            error
        );
    }

    let state = read_icd_state(node_id, context).await?;
    Ok(serde_json::to_value(state).unwrap_or(Value::Null))
}

/// Drop this controller's registration.
///
/// `force` skips the peer round-trip, for a device that is already gone.
pub async fn unregister_icd(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let force = args.bool_or("force", false)?;
    let fabric = context.server.fabric_info().await?;

    // The key stops being useful the moment the device forgets it, and a
    // forced unregistration means the device is gone; either way, keeping a
    // secret nobody will ever send is worse than dropping it.
    if let Err(error) = context.server.config.remove_icd_registration(node_id) {
        log::warn!(
            "Could not drop node {}'s check-in key: {}",
            node_id,
            error
        );
    }

    if !force {
        context
            .server
            .matter
            .invoke(
                node_id,
                0,
                ICD_MANAGEMENT_CLUSTER,
                UNREGISTER_CLIENT_COMMAND,
                TlvNode::Struct(vec![(0, TlvNode::U64(fabric.node_id))]),
                None,
                BTreeMap::new(),
            )
            .await?;
        let state = read_icd_state(node_id, context).await?;
        return Ok(serde_json::to_value(state).unwrap_or(Value::Null));
    }

    // Forced: report what is locally known without touching the peer.
    let available = context.server.nodes.get(node_id).map(|node| node.available);
    Ok(serde_json::to_value(IcdStateData {
        supported: true,
        lit_supported: false,
        registered: false,
        operating_mode: None,
        awake: None,
        available,
        next_expected_checkin: None,
    })
    .unwrap_or(Value::Null))
}

/// Drop the registration and reconnect.
///
/// This is the last resort for a LIT device that has stopped answering: the
/// registration is removed and a fresh session is attempted, after which a LIT
/// peer re-registers on its own once subscribed.
pub async fn resync_icd(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let fabric = context.server.fabric_info().await?;

    // Both steps are best effort: the point of a resync is that the peer is
    // already misbehaving.
    let _ = context
        .server
        .matter
        .invoke(
            node_id,
            0,
            ICD_MANAGEMENT_CLUSTER,
            UNREGISTER_CLIENT_COMMAND,
            TlvNode::Struct(vec![(0, TlvNode::U64(fabric.node_id))]),
            None,
            BTreeMap::new(),
        )
        .await;
    let _ = context.server.matter.ping(node_id).await;
    Ok(Value::Null)
}

/// Vendor ids of administrator fabrics on the peer that are not ours.
async fn other_vendor_admins(
    node_id: u64,
    ignored_vendors: &[u64],
    context: CallContext<'_>,
) -> Result<Vec<u16>, ApiError> {
    let fabric = context.server.fabric_info().await?;
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

    let Some(entries) = attributes
        .get(&format!(
            "0/{}/{}",
            OPERATIONAL_CREDENTIALS_CLUSTER, FABRICS_ATTRIBUTE
        ))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };

    let mut others: Vec<u16> = entries
        .iter()
        .filter_map(|entry| entry.get(FABRIC_VENDOR_ID_TAG).and_then(Value::as_u64))
        .filter(|vendor| *vendor != fabric.vendor_id as u64 && !ignored_vendors.contains(vendor))
        .map(|vendor| vendor as u16)
        .collect();
    others.sort_unstable();
    others.dedup();
    Ok(others)
}

/// Fill a buffer with random bytes for the check-in key.
fn getrandom_key(key: &mut [u8]) {
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::{call, test_context_with_fabric};
    use crate::protocol::model::MatterNodeData;
    use crate::storage::StoredNode;
    use futures_lite::future::block_on;
    use serde_json::json;

    fn context_with_node() -> crate::api::ServerContext {
        let context = test_context_with_fabric();
        context.nodes.upsert(StoredNode::new(MatterNodeData::new(
            1,
            "2026-01-01T00:00:00.000Z".into(),
        )));
        context
    }

    /// A device is awake for its active period after checking in, and not
    /// after that.
    #[test]
    fn awake_follows_the_active_mode_duration() {
        let just_now = SystemTime::now();
        assert_eq!(awake(Some(just_now), Some(4_000)), Some(true));

        let a_while_ago = just_now - Duration::from_secs(30);
        assert_eq!(awake(Some(a_while_ago), Some(4_000)), Some(false));
    }

    /// Not knowing is reported as not knowing: a device that has not checked
    /// in since this server started, or one that does not say how long it
    /// stays awake, must not be guessed at.
    #[test]
    fn awake_is_unknown_without_a_check_in_or_a_duration() {
        assert_eq!(awake(None, Some(4_000)), None);
        assert_eq!(awake(Some(SystemTime::now()), None), None);
    }

    #[test]
    fn the_next_check_in_is_one_idle_period_after_the_last() {
        // 2026-01-01T00:00:00Z, in epoch milliseconds.
        let last = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        assert_eq!(
            next_expected_checkin(Some(last), Some(3_600)),
            Some((1_767_225_600 + 3_600) * 1_000)
        );
        assert_eq!(next_expected_checkin(None, Some(3_600)), None);
        assert_eq!(next_expected_checkin(Some(last), None), None);
    }

    #[test]
    fn a_node_without_the_cluster_reports_unsupported() {
        let context = context_with_node();
        // The actor is not running, so the read fails the way an absent
        // cluster does; the state must still be a well-formed "unsupported".
        let args = Args::new(json!({ "node_id": 1 }));
        let state = block_on(get_icd_state(&args, call(&context))).unwrap();
        assert_eq!(state["supported"], json!(false));
        assert_eq!(state["lit_supported"], json!(false));
        assert_eq!(state["registered"], json!(false));
        assert_eq!(state["operating_mode"], Value::Null);
        assert_eq!(state["next_expected_checkin"], Value::Null);
    }

    #[test]
    fn icd_commands_require_a_known_node() {
        let context = context_with_node();
        let args = Args::new(json!({ "node_id": 99 }));
        for result in [
            block_on(get_icd_state(&args, call(&context))),
            block_on(register_icd(&args, call(&context))),
            block_on(unregister_icd(&args, call(&context))),
            block_on(resync_icd(&args, call(&context))),
        ] {
            assert_eq!(result.unwrap_err().code.as_i64(), 5);
        }
    }

    #[test]
    fn a_forced_unregister_does_not_need_the_peer() {
        let context = context_with_node();
        let args = Args::new(json!({ "node_id": 1, "force": true }));
        let state = block_on(unregister_icd(&args, call(&context))).unwrap();
        assert_eq!(state["registered"], json!(false));
        assert_eq!(state["available"], json!(true));
    }

    #[test]
    fn the_multi_admin_error_carries_the_vendor_list() {
        let error = ApiError::icd_multi_admin(&[4874, 65521]);
        assert_eq!(error.code.as_i64(), 100);
        let details: Value = serde_json::from_str(&error.details).unwrap();
        assert_eq!(details["admin_vendor_ids"], json!([4874, 65521]));
        assert!(details["message"].as_str().unwrap().contains("LIT"));
    }

    #[test]
    fn check_in_keys_are_sixteen_random_bytes() {
        let mut first = vec![0u8; ICD_KEY_LEN];
        let mut second = vec![0u8; ICD_KEY_LEN];
        getrandom_key(&mut first);
        getrandom_key(&mut second);
        assert_eq!(first.len(), 16);
        assert_ne!(first, second);
        assert!(first.iter().any(|byte| *byte != 0));
    }
}
