//! Commissioning: bringing a device onto this controller's fabric, and sharing
//! a commissioned device with another controller.

use std::collections::BTreeMap;

use rs_matter::crypto::{CryptoSensitive, CryptoSensitiveRef};
use rs_matter::pairing::qr::{no_optional_data, CommFlowType, QrPayload};
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::BasicCommData;
use serde_json::Value;

use crate::matter::actor::CommissionTarget;
use crate::matter::commissioning::{
    parse_pairing_code, NetworkCredentials, ThreadCredentials, WifiCredentials,
};
use crate::matter::mdns_browser::{self, ServiceInstance};
use crate::matter::spake2p_verifier::{DEFAULT_ITERATIONS, MAX_SALT_LEN};
use crate::matter::tlv_json::TlvNode;
use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::events::Event;
use crate::protocol::message::Args;
use crate::protocol::model::{CommissionableNodeData, CommissioningParameters, MatterNodeData};
use crate::storage::{thread_dataset, StoredNode};

use super::{fabrics, nodes, now_iso, require_node, CallContext};

/// A setup passcode is a little-endian `u32`. rs-matter names the type in a
/// private module, so it is spelled out here as the public canonical type.
type SetupPasscode = CryptoSensitive<4>;

/// How long discovery waits for a matching device to answer.
const DISCOVERY_TIMEOUT_MS: u32 = 15_000;
/// How many devices a `discover` sweep reports. rs-matter's browse can step
/// past at most this many already-seen instances.
const DISCOVERY_LIMIT: usize = 6;

const ADMINISTRATOR_COMMISSIONING_CLUSTER: u32 = 60;
const OPEN_COMMISSIONING_WINDOW_COMMAND: u32 = 0;
/// Commands on this cluster must be sent as timed invokes.
const COMMISSIONING_WINDOW_TIMED_TIMEOUT_MS: u16 = 10_000;
const DEFAULT_COMMISSIONING_WINDOW_SECS: u64 = 300;

/// Commission a device from a QR or manual pairing code.
pub async fn commission_with_code(args: &Args, context: CallContext<'_>) -> ApiResult {
    let code = args.req_str("code")?;
    let network_only = args.bool_or("network_only", false)?;
    let pairing = parse_pairing_code(code)?;

    // A device already on the network is found over mDNS, which is quicker
    // and needs no radio, so that is always tried first. Bluetooth is the
    // fallback for a factory-fresh wireless device, which has no network to
    // be found on yet.
    let bluetooth = !network_only && context.server.runtime.bluetooth_enabled;
    if !bluetooth {
        log::info!(
            "Bluetooth is unavailable; commissioning will only find devices already on the network"
        );
    }

    let credentials = network_credentials(&context);
    let filter = pairing.filter.clone();
    let over_network = commission(
        context,
        CommissionTarget::Discovered {
            filter: pairing.filter,
            timeout_ms: DISCOVERY_TIMEOUT_MS,
        },
        pairing.passcode,
    )
    .await;

    match over_network {
        Ok(result) => Ok(result),
        Err(error) if bluetooth => {
            log::info!("Not found on the network ({error}); scanning over Bluetooth");
            commission(
                context,
                CommissionTarget::Bluetooth {
                    filter,
                    timeout_secs: BLUETOOTH_SCAN_TIMEOUT_SECS,
                    credentials: Some(credentials),
                },
                pairing.passcode,
            )
            .await
        }
        Err(error) => Err(error),
    }
}

/// How long to scan for a commissionable Bluetooth advertisement.
///
/// A factory-fresh device advertises for about fifteen minutes, so the limit
/// here is the caller's patience rather than the device's window.
const BLUETOOTH_SCAN_TIMEOUT_SECS: u16 = 30;

/// The credentials a Bluetooth-commissioned device may need to join a network.
///
/// Both kinds are collected; the device's `NetworkCommissioning` feature map
/// decides which one is actually sent.
fn network_credentials(context: &CallContext<'_>) -> NetworkCredentials {
    let store = &context.server.config;

    let wifi = store
        .wifi_credentials(None)
        .map(|(ssid, password)| WifiCredentials { ssid, password });

    let thread = store.thread_dataset(None).and_then(|hex| {
        let decoded = thread_dataset::decode(&hex)?;
        Some(ThreadCredentials {
            dataset: thread_dataset::from_hex(&hex)?,
            ext_pan_id: decoded.ext_pan_id_bytes()?,
        })
    });

    NetworkCredentials { wifi, thread }
}

/// Commission a device already on the IP network.
pub async fn commission_on_network(args: &Args, context: CallContext<'_>) -> ApiResult {
    let passcode = args.req_u32("setup_pin_code")?;
    let target = match args.str("ip_addr")? {
        Some(address) if !address.is_empty() => CommissionTarget::Address {
            address: address.to_string(),
        },
        _ => CommissionTarget::Discovered {
            filter: discovery_filter(args)?,
            timeout_ms: DISCOVERY_TIMEOUT_MS,
        },
    };
    commission(context, target, passcode).await
}

/// The shared tail of both commissioning commands: allocate an id, run the
/// flow, interview the device, persist it, and announce it.
///
/// The node is stored before the interview so a process exit mid-interview
/// leaves a node that can be re-interviewed rather than a device that is on
/// the fabric but invisible to the controller.
async fn commission(
    context: CallContext<'_>,
    target: CommissionTarget,
    passcode: u32,
) -> ApiResult {
    let node_id = context
        .server
        .config
        .allocate_node_id()
        .map_err(|e| ApiError::sdk(format!("Failed to allocate a node id: {}", e)))?;

    let (node_id, address, device_fabric_index) = context
        .server
        .matter
        .commission(target, passcode, node_id)
        .await?;

    let node = MatterNodeData::new(node_id, now_iso());
    let mut stored = StoredNode::new(node);
    // Bluetooth commissioning reports no address of its own: a BT MAC is not
    // an operational address, and rs-matter does not expose the one it
    // resolved to finish over CASE. The node is announcing by now, so it is
    // resolved below — but the record has to exist first for that to have
    // somewhere to go.
    stored.ip_addresses = if address.is_empty() {
        Vec::new()
    } else {
        vec![address]
    };
    stored.device_fabric_index = Some(device_fabric_index);
    context.server.nodes.upsert(stored);
    context
        .server
        .nodes
        .save()
        .map_err(|e| ApiError::sdk(format!("Failed to persist the new node: {}", e)))?;

    // A node commissioned over Bluetooth has just joined its network, so this
    // is the first moment it can be found by the name it will keep. Best
    // effort: an address that cannot be resolved now is resolved by the next
    // `get_node_ip_addresses`, and commissioning has already succeeded either
    // way.
    if context.server.nodes.ip_addresses(node_id).is_empty() {
        nodes::resolve_addresses(context.server, node_id).await;
    }

    // Tell the device what to call this fabric, so a user listing fabrics on
    // it from another ecosystem sees a name rather than a blank. Best effort:
    // a device that refuses the label is still commissioned.
    let label = context.server.config.fabric_label();
    if let Err(error) = fabrics::push_fabric_label(context, node_id, &label).await {
        log::warn!(
            "Node {} would not accept the fabric label '{}': {}",
            node_id,
            label,
            error
        );
    }

    // The first interview populates the attributes clients need to build
    // entities, so `node_added` is published after it rather than before.
    let node = match context.server.matter.interview(node_id).await {
        Ok(attributes) => nodes::apply_interview(context.server, node_id, attributes)?,
        Err(error) => {
            log::warn!(
                "Node {} was commissioned but its first interview failed: {}",
                node_id,
                error
            );
            context
                .server
                .nodes
                .get(node_id)
                .ok_or_else(|| ApiError::node_not_exists(node_id))?
        }
    };

    context.server.events.publish(Event::node_added(&node));
    Ok(serde_json::to_value(node).unwrap_or(Value::Null))
}

/// Open a commissioning window so another controller can join the device.
///
/// The window is *enhanced*: a fresh passcode is generated here, only its PAKE
/// verifier is sent to the device, and the passcode itself is returned to the
/// caller in the pairing codes.
pub async fn open_commissioning_window(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let timeout = args
        .u64("timeout")?
        .unwrap_or(DEFAULT_COMMISSIONING_WINDOW_SECS);
    if !(180..=900).contains(&timeout) {
        return Err(ApiError::invalid_args(
            "timeout must be between 180 and 900 seconds",
        ));
    }
    let iterations = args.u32("iteration")?.unwrap_or(DEFAULT_ITERATIONS);
    let discriminator = match args.u16("discriminator")? {
        Some(value) if value < 4096 => value,
        Some(_) => {
            return Err(ApiError::invalid_args(
                "discriminator must be a 12-bit value",
            ))
        }
        None => random_discriminator(),
    };

    let passcode = random_passcode();
    let salt = random_salt();
    let verifier = context
        .server
        .matter
        .pake_verifier(passcode, salt.clone(), iterations)
        .await?;

    context
        .server
        .matter
        .invoke(
            node_id,
            0,
            ADMINISTRATOR_COMMISSIONING_CLUSTER,
            OPEN_COMMISSIONING_WINDOW_COMMAND,
            TlvNode::Struct(vec![
                (0, TlvNode::U64(timeout)),
                (1, TlvNode::Bytes(verifier)),
                (2, TlvNode::U64(discriminator as u64)),
                (3, TlvNode::U64(iterations as u64)),
                (4, TlvNode::Bytes(salt)),
            ]),
            Some(COMMISSIONING_WINDOW_TIMED_TIMEOUT_MS),
            BTreeMap::new(),
        )
        .await?;

    let parameters = pairing_codes(context, node_id, passcode, discriminator)?;
    Ok(serde_json::to_value(parameters).unwrap_or(Value::Null))
}

/// Build the manual and QR pairing codes a user needs to add the device
/// elsewhere. The vendor and product ids come from the node's own Basic
/// Information, so the QR code identifies the right device.
fn pairing_codes(
    context: CallContext<'_>,
    node_id: u64,
    passcode: u32,
    discriminator: u16,
) -> Result<CommissioningParameters, ApiError> {
    let node = context
        .server
        .nodes
        .get(node_id)
        .ok_or_else(|| ApiError::node_not_exists(node_id))?;

    let vendor_id = node
        .attributes
        .get("0/40/2")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u16;
    let product_id = node
        .attributes
        .get("0/40/4")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u16;
    let serial = node.basic_info_string(15).unwrap_or("");

    let passcode_bytes = passcode.to_le_bytes();
    let comm_data = BasicCommData {
        password: SetupPasscode::new_from_ref(CryptoSensitiveRef::new(&passcode_bytes)),
        discriminator,
    };
    let manual = comm_data.compute_pairing_code();

    let qr_payload = QrPayload::new(
        DiscoveryCapabilities::IP,
        CommFlowType::Standard,
        BasicCommData {
            password: SetupPasscode::new_from_ref(CryptoSensitiveRef::new(&passcode_bytes)),
            discriminator,
        },
        vendor_id,
        product_id,
        serial,
        no_optional_data,
    );
    let mut buf = [0u8; 256];
    let qr = qr_payload
        .as_str(&mut buf)
        .map(|(qr, _)| qr.to_string())
        .map_err(|e| ApiError::sdk(format!("Could not build the QR code: {:?}", e.code())))?;

    Ok(CommissioningParameters {
        setup_pin_code: passcode,
        setup_manual_code: manual.to_string(),
        setup_qr_code: qr,
    })
}

/// The mDNS service commissionable Matter devices advertise.
const COMMISSIONABLE_SERVICE: &str = "_matterc._udp.local";
/// How long the discovery browse listens.
const BROWSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(3000);

/// Discover commissionable devices on the network.
///
/// This browses mDNS directly rather than going through the Matter actor:
/// rs-matter's browse is a rendezvous built for commissioning and reports only
/// an address, while the protocol's `CommissionableNodeData` is mostly TXT
/// record content. Commissioning itself still uses rs-matter's browse, which is
/// what feeds the commissioner an address.
pub async fn discover(args: &Args, context: CallContext<'_>) -> ApiResult {
    let filter = discovery_filter(args)?;

    let instances = match mdns_browser::browse(COMMISSIONABLE_SERVICE, BROWSE_TIMEOUT).await {
        Ok(instances) => instances,
        Err(error) => {
            log::warn!("Could not browse for commissionable devices: {}", error);
            // Fall back to the Matter stack's own browse, which at least
            // reports an address.
            let nodes = context
                .server
                .matter
                .discover(filter, DISCOVERY_TIMEOUT_MS, DISCOVERY_LIMIT)
                .await?;
            return Ok(serde_json::to_value(nodes).unwrap_or(Value::Null));
        }
    };

    let found: Vec<CommissionableNodeData> = instances
        .iter()
        .map(commissionable_node)
        .filter(|node| matches_filter(node, &filter))
        .collect();
    Ok(serde_json::to_value(found).unwrap_or(Value::Null))
}

/// Map a `_matterc._udp` answer onto the wire shape. The TXT keys are the ones
/// the Matter specification defines for commissionable discovery.
fn commissionable_node(instance: &ServiceInstance) -> CommissionableNodeData {
    // `VP` is "vendor+product", either of which may be absent.
    let (vendor_id, product_id) = match instance.txt_str("VP") {
        Some(vp) => {
            let mut parts = vp.split('+');
            (
                parts.next().and_then(|value| value.parse::<u16>().ok()),
                parts.next().and_then(|value| value.parse::<u16>().ok()),
            )
        }
        None => (None, None),
    };

    CommissionableNodeData {
        instance_name: Some(instance.instance_name.clone()),
        host_name: instance.host_name.clone(),
        port: instance.port,
        long_discriminator: instance.txt_u32("D").map(|value| value as u16),
        vendor_id,
        product_id,
        commissioning_mode: instance.txt_u32("CM").map(|value| value as u8),
        device_type: instance.txt_u32("DT"),
        device_name: instance.txt_str("DN"),
        pairing_instruction: instance.txt_str("PI"),
        pairing_hint: instance.txt_u32("PH"),
        mrp_retry_interval_idle: instance.txt_u32("SII"),
        mrp_retry_interval_active: instance.txt_u32("SAI"),
        supports_tcp: instance.txt_u32("T").map(|value| value != 0),
        addresses: Some(
            instance
                .addresses
                .iter()
                .map(|address| address.to_string())
                .collect(),
        ),
        rotating_id: instance.txt_str("RI"),
    }
}

/// Apply the caller's filter to a discovered device.
///
/// A device that does not advertise the field being filtered on is kept: the
/// TXT record is optional, and dropping such a device would hide something the
/// caller could still commission.
fn matches_filter(node: &CommissionableNodeData, filter: &CommissionableFilter) -> bool {
    let matches = |wanted: Option<u64>, actual: Option<u64>| match (wanted, actual) {
        (Some(wanted), Some(actual)) => wanted == actual,
        (Some(_), None) => true,
        (None, _) => true,
    };

    if let (Some(short), Some(long)) = (filter.short_discriminator, node.long_discriminator) {
        // A manual pairing code carries only the top four bits.
        if (long >> 8) as u8 != short {
            return false;
        }
    }
    if filter.commissioning_mode_only && node.commissioning_mode == Some(0) {
        return false;
    }

    matches(
        filter.discriminator.map(u64::from),
        node.long_discriminator.map(u64::from),
    ) && matches(
        filter.vendor_id.map(u64::from),
        node.vendor_id.map(u64::from),
    ) && matches(
        filter.product_id.map(u64::from),
        node.product_id.map(u64::from),
    ) && matches(
        filter.device_type.map(u64::from),
        node.device_type.map(u64::from),
    )
}

/// Build a discovery filter from either the `commission_on_network` filter
/// pair or the individual fields `discover` accepts.
fn discovery_filter(args: &Args) -> Result<CommissionableFilter, ApiError> {
    let mut filter = CommissionableFilter {
        discriminator: args.u16("discriminator")?,
        vendor_id: args.u16("vendor_id")?,
        product_id: args.u16("product_id")?,
        device_type: args.u32("device_type")?,
        commissioning_mode_only: args.bool_or("commissioning_mode_only", false)?,
        ..CommissionableFilter::default()
    };

    if let Some(filter_type) = args.u64("filter_type")? {
        let value = args.u64("filter")?;
        match filter_type {
            // 0 is "no filter"; the value, if any, is ignored.
            0 => {}
            1 => {
                filter.short_discriminator =
                    value.map(|value| u8::try_from(value & 0xF).unwrap_or_default())
            }
            2 => {
                filter.discriminator = value
                    .map(u16::try_from)
                    .transpose()
                    .map_err(|_| ApiError::invalid_args("Invalid discriminator filter"))?
            }
            3 => {
                filter.vendor_id = value
                    .map(u16::try_from)
                    .transpose()
                    .map_err(|_| ApiError::invalid_args("Invalid vendor id filter"))?
            }
            4 => {
                filter.device_type = value
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| ApiError::invalid_args("Invalid device type filter"))?
            }
            other => {
                return Err(ApiError::invalid_args(format!(
                    "Unsupported filter_type {}",
                    other
                )))
            }
        }
    }
    Ok(filter)
}

/// Passcodes the Matter spec forbids, because they are trivially guessable.
const FORBIDDEN_PASSCODES: &[u32] = &[
    0, 11111111, 22222222, 33333333, 44444444, 55555555, 66666666, 77777777, 88888888, 99999999,
    12345678, 87654321,
];

fn random_passcode() -> u32 {
    use rand_core::RngCore;
    loop {
        // The valid range is 1..=99_999_998, minus the forbidden values.
        let candidate = rand_core::OsRng.next_u32() % 99_999_999;
        if candidate != 0 && !FORBIDDEN_PASSCODES.contains(&candidate) {
            return candidate;
        }
    }
}

fn random_discriminator() -> u16 {
    use rand_core::RngCore;
    (rand_core::OsRng.next_u32() & 0xFFF) as u16
}

fn random_salt() -> Vec<u8> {
    use rand_core::RngCore;
    let mut salt = vec![0u8; MAX_SALT_LEN];
    rand_core::OsRng.fill_bytes(&mut salt);
    salt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::{call, test_context_with_fabric};
    use futures_lite::future::block_on;
    use serde_json::json;

    #[test]
    fn a_missing_code_is_an_argument_error() {
        let context = test_context_with_fabric();
        let error = block_on(commission_with_code(&Args::default(), call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(error.details.contains("Missing code"));
    }

    #[test]
    fn a_malformed_code_is_rejected_before_any_radio_work() {
        let context = test_context_with_fabric();
        let args = Args::new(json!({ "code": "nonsense" }));
        let error = block_on(commission_with_code(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
    }

    #[test]
    fn filter_types_map_onto_the_right_filter_field() {
        let short =
            discovery_filter(&Args::new(json!({ "filter_type": 1, "filter": 15 }))).unwrap();
        assert_eq!(short.short_discriminator, Some(15));
        assert_eq!(short.discriminator, None);

        let long =
            discovery_filter(&Args::new(json!({ "filter_type": 2, "filter": 3840 }))).unwrap();
        assert_eq!(long.discriminator, Some(3840));

        let vendor =
            discovery_filter(&Args::new(json!({ "filter_type": 3, "filter": 4874 }))).unwrap();
        assert_eq!(vendor.vendor_id, Some(4874));

        let device =
            discovery_filter(&Args::new(json!({ "filter_type": 4, "filter": 22 }))).unwrap();
        assert_eq!(device.device_type, Some(22));

        let none = discovery_filter(&Args::new(json!({ "filter_type": 0, "filter": 99 }))).unwrap();
        assert_eq!(none, CommissionableFilter::default());
    }

    #[test]
    fn an_unknown_filter_type_is_rejected() {
        let error =
            discovery_filter(&Args::new(json!({ "filter_type": 9, "filter": 1 }))).unwrap_err();
        assert!(error.details.contains("Unsupported filter_type"));
    }

    #[test]
    fn commissioning_windows_bound_their_timeout_and_discriminator() {
        let context = test_context_with_fabric();
        context.nodes.upsert(StoredNode::new(MatterNodeData::new(
            1,
            "2026-01-01T00:00:00.000Z".into(),
        )));

        let args = Args::new(json!({ "node_id": 1, "timeout": 60 }));
        assert!(block_on(open_commissioning_window(&args, call(&context)))
            .unwrap_err()
            .details
            .contains("between 180 and 900"));

        let args = Args::new(json!({ "node_id": 1, "discriminator": 5000 }));
        assert!(block_on(open_commissioning_window(&args, call(&context)))
            .unwrap_err()
            .details
            .contains("12-bit"));
    }

    #[test]
    fn commissionable_answers_map_onto_the_wire_shape() {
        let mut instance = ServiceInstance {
            instance_name: "A1B2C3D4E5F60718".into(),
            host_name: Some("plug.local".into()),
            port: Some(5540),
            addresses: vec!["fd00::2".parse().unwrap()],
            ..ServiceInstance::default()
        };
        instance.txt.insert("D".into(), b"3840".to_vec());
        instance.txt.insert("VP".into(), b"65521+32769".to_vec());
        instance.txt.insert("CM".into(), b"1".to_vec());
        instance.txt.insert("DT".into(), b"266".to_vec());
        instance.txt.insert("DN".into(), b"Smart Plug".to_vec());
        instance.txt.insert("PH".into(), b"33".to_vec());
        instance.txt.insert("T".into(), b"1".to_vec());

        let node = commissionable_node(&instance);
        assert_eq!(node.instance_name.as_deref(), Some("A1B2C3D4E5F60718"));
        assert_eq!(node.long_discriminator, Some(3840));
        assert_eq!(node.vendor_id, Some(0xFFF1));
        assert_eq!(node.product_id, Some(0x8001));
        assert_eq!(node.commissioning_mode, Some(1));
        assert_eq!(node.device_type, Some(266));
        assert_eq!(node.device_name.as_deref(), Some("Smart Plug"));
        assert_eq!(node.pairing_hint, Some(33));
        assert_eq!(node.supports_tcp, Some(true));
        assert_eq!(node.addresses.as_ref().unwrap(), &["fd00::2".to_string()]);
    }

    #[test]
    fn filters_are_applied_to_discovered_devices() {
        let mut node = CommissionableNodeData {
            long_discriminator: Some(3840),
            vendor_id: Some(0xFFF1),
            product_id: Some(0x8001),
            device_type: Some(266),
            commissioning_mode: Some(1),
            ..CommissionableNodeData::default()
        };

        let matching = CommissionableFilter {
            discriminator: Some(3840),
            vendor_id: Some(0xFFF1),
            ..CommissionableFilter::default()
        };
        assert!(matches_filter(&node, &matching));

        let other_vendor = CommissionableFilter {
            vendor_id: Some(0x1234),
            ..CommissionableFilter::default()
        };
        assert!(!matches_filter(&node, &other_vendor));

        // A manual code's short discriminator is the top four bits.
        let short = CommissionableFilter {
            short_discriminator: Some(0x0F),
            ..CommissionableFilter::default()
        };
        assert!(matches_filter(&node, &short));
        let wrong_short = CommissionableFilter {
            short_discriminator: Some(0x01),
            ..CommissionableFilter::default()
        };
        assert!(!matches_filter(&node, &wrong_short));

        // A device not in commissioning mode is excluded when asked for.
        node.commissioning_mode = Some(0);
        let commissioning_only = CommissionableFilter {
            commissioning_mode_only: true,
            ..CommissionableFilter::default()
        };
        assert!(!matches_filter(&node, &commissioning_only));
    }

    #[test]
    fn a_device_that_omits_a_filtered_field_is_kept() {
        // The TXT record is optional; dropping such a device would hide
        // something the caller could still commission.
        let node = CommissionableNodeData::default();
        let filter = CommissionableFilter {
            vendor_id: Some(0xFFF1),
            ..CommissionableFilter::default()
        };
        assert!(matches_filter(&node, &filter));
    }

    #[test]
    fn generated_passcodes_avoid_the_forbidden_values() {
        for _ in 0..200 {
            let passcode = random_passcode();
            assert!(!FORBIDDEN_PASSCODES.contains(&passcode));
            assert!((1..=99_999_998).contains(&passcode));
        }
    }

    #[test]
    fn generated_discriminators_are_twelve_bit() {
        for _ in 0..100 {
            assert!(random_discriminator() < 4096);
        }
    }

    #[test]
    fn pairing_codes_round_trip_through_the_parser() {
        let context = test_context_with_fabric();
        let mut node = MatterNodeData::new(1, "2026-01-01T00:00:00.000Z".into());
        node.attributes.insert("0/40/2".into(), json!(0xFFF1));
        node.attributes.insert("0/40/4".into(), json!(0x8001));
        context.nodes.upsert(StoredNode::new(node));

        let parameters = pairing_codes(call(&context), 1, 20202021, 3840).unwrap();
        assert_eq!(parameters.setup_pin_code, 20202021);
        assert!(parameters.setup_qr_code.starts_with("MT:"));

        // Both codes must lead a client back to the same passcode.
        let from_manual = parse_pairing_code(&parameters.setup_manual_code).unwrap();
        assert_eq!(from_manual.passcode, 20202021);
        let from_qr = parse_pairing_code(&parameters.setup_qr_code).unwrap();
        assert_eq!(from_qr.passcode, 20202021);
        assert_eq!(from_qr.filter.discriminator, Some(3840));
    }
}
