//! Pairing-code parsing and the commissioning flow.
//!
//! A pairing code carries two things the controller needs: the setup passcode,
//! and enough of the device's identity to find it on the network. rs-matter's
//! payload parsers produce both, including the discriminator filter — a QR code
//! carries the full 12-bit discriminator while a manual code carries only its
//! top 4 bits, and the filter records which, so discovery matches the same way
//! the reference does.

use std::num::NonZeroU8;

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::{CanonPkcSecretKey, Crypto};
use rs_matter::dm::clusters::decl::network_commissioning::{
    Feature, NetworkCommissioningClient, NetworkCommissioningStatusEnum,
};
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::onboard::{CommissionOptions, Commissioner};
use rs_matter::pairing::qr::QrPayload;
use rs_matter::tlv::OctetStr;
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::Address;
use rs_matter::Matter;

use crate::protocol::error::ApiError;

/// What a pairing code resolves to.
#[derive(Clone, Debug)]
pub struct PairingInfo {
    pub passcode: u32,
    /// Discovery filter matching this device: a long discriminator from a QR
    /// code, a short one from a manual code.
    pub filter: CommissionableFilter,
}

/// How to reach the device for phase 2, once AddNOC has been accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CasePath {
    /// The address PASE used still reaches the device, so CASE runs against
    /// it directly. True of anything commissioned over IP.
    SameAddress,
    /// Resolve the device's operational address over mDNS first. Required
    /// after Bluetooth commissioning: the device joins its network during
    /// commissioning, so the address it will answer on is not known until it
    /// announces itself operationally.
    Operational,
}

/// Credentials for the network a device should be told to join.
///
/// Both kinds are carried rather than one, because which one applies is a
/// property of the device: a controller can hold Wi-Fi and Thread credentials
/// at once, and only the device's `NetworkCommissioning` feature map says
/// which it can use.
#[derive(Clone, Debug, Default)]
pub struct NetworkCredentials {
    pub wifi: Option<WifiCredentials>,
    pub thread: Option<ThreadCredentials>,
}

#[derive(Clone, Debug)]
pub struct WifiCredentials {
    pub ssid: String,
    pub password: String,
}

#[derive(Clone, Debug)]
pub struct ThreadCredentials {
    /// The operational dataset, as raw MeshCoP TLV.
    pub dataset: Vec<u8>,
    /// The extended PAN id, which is how `ConnectNetwork` names a Thread
    /// network — the equivalent of an SSID.
    pub ext_pan_id: Vec<u8>,
}

impl NetworkCredentials {
    pub fn is_empty(&self) -> bool {
        self.wifi.is_none() && self.thread.is_none()
    }
}

/// What happens after AddNOC is accepted.
///
/// The two travel together because both answer the same question — how to
/// finish once the device holds its operational certificate — and splitting
/// them across separate parameters made the signature unreadable.
pub struct Completion<'a> {
    pub case_path: CasePath,
    /// The network to hand the device, for one that does not have one yet.
    pub credentials: Option<&'a NetworkCredentials>,
}

/// What a completed commissioning produced.
#[derive(Clone, Copy, Debug)]
pub struct CommissionOutcome {
    pub node_id: u64,
    /// The fabric slot the device assigned to us, needed to remove ourselves
    /// from it later.
    pub device_fabric_index: u8,
}

/// Parse a QR (`MT:`) or manual pairing code.
pub fn parse_pairing_code(code: &str) -> Result<PairingInfo, ApiError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(ApiError::invalid_args("Missing code"));
    }

    if let Some(rest) = code.strip_prefix("MT:") {
        let _ = rest;
        let mut buf = [0u8; 256];
        let payload = QrPayload::parse(code, &mut buf).map_err(|e| {
            ApiError::invalid_args(format!("Invalid QR pairing code: {:?}", e.code()))
        })?;
        return Ok(PairingInfo {
            passcode: payload.passcode(),
            filter: payload.commissionable_filter(),
        });
    }

    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != code.chars().filter(|c| !matches!(c, '-' | ' ')).count() {
        return Err(ApiError::invalid_args(format!(
            "Unrecognized pairing code format: {}",
            code
        )));
    }

    let payload = QrPayload::parse_pairing_code(&digits).map_err(|e| {
        ApiError::invalid_args(format!("Invalid manual pairing code: {:?}", e.code()))
    })?;
    Ok(PairingInfo {
        passcode: payload.passcode(),
        filter: payload.commissionable_filter(),
    })
}

/// Run PASE, AddNOC, CASE and CommissioningComplete against a device at a
/// known address.
pub async fn commission_at_address<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    issuer_private_key: &CanonPkcSecretKey,
    peer_addr: Address,
    passcode: u32,
    node_id: u64,
    completion: Completion<'_>,
) -> Result<CommissionOutcome, ApiError> {
    let fab_idx: NonZeroU8 = matter
        .with_state(|state| state.fabrics.iter().next().map(|f| f.fab_idx()))
        .ok_or_else(|| ApiError::sdk("No fabric is installed on the controller"))?;

    // The NOC generator only caches certificate metadata, so it is rebuilt per
    // request; that keeps its scratch buffer on this stack frame instead of in
    // a self-referential long-lived structure, and the actor already
    // serializes the requests that use it.
    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_generator = matter
        .with_state(|state| {
            let fabric = state.fabrics.fabric(fab_idx)?;
            // Two shapes of fabric reach this line. One created here has an
            // ICAC and signs with the ICAC key; one imported from
            // matterjs-server usually has none and signs with the root key.
            // rs-matter picks the issuer from whether `icac` is empty, so both
            // work as long as the stored key is the one that matches.
            NocGenerator::create(
                issuer_private_key.reference(),
                fabric.root_ca(),
                fabric.icac(),
                &mut noc_buf,
            )
        })
        .map_err(|e| ApiError::sdk(format!("Failed to prepare NOC issuance: {:?}", e.code())))?;

    let mut commissioner_buf = [0u8; MAX_CERT_TLV_LEN];
    let mut commissioner = Commissioner::new(
        matter,
        crypto,
        fab_idx,
        &mut noc_generator,
        &mut commissioner_buf,
    );
    let options = CommissionOptions {
        // rs-matter has no DCL-backed attestation verification yet, and
        // rejecting every device instead would make commissioning impossible.
        allow_test_attestation: true,
        ..CommissionOptions::default()
    };

    let phase1 = commissioner
        .commission(peer_addr, passcode, &options, node_id, VALID_FOREVER)
        .await
        .map_err(|e| {
            ApiError::commission_failed(format!("Commissioning failed: {:?}", e.code()))
        })?;
    // A device reached over Bluetooth has no network yet, so it has to be
    // given one before phase 2 can reach it over IP at all. This sits between
    // AddNOC and CASE because the Matter spec puts it there: the fail-safe is
    // still armed and the PASE session is still the only way to talk.
    if let Some(credentials) = completion.credentials {
        provision_network(matter, crypto, peer_addr, passcode, credentials).await?;
    }

    match completion.case_path {
        CasePath::SameAddress => commissioner.complete_via_case(peer_addr, &phase1).await,
        CasePath::Operational => commissioner.complete_via_case_operational(&phase1).await,
    }
    .map_err(|e| {
        ApiError::commission_failed(format!(
            "Commissioning could not be completed over CASE: {:?}",
            e.code()
        ))
    })?;

    Ok(CommissionOutcome {
        node_id: phase1.device_node_id,
        device_fabric_index: phase1.fabric_index.get(),
    })
}

/// Hand a device its network credentials over the existing PASE session.
///
/// `Exchange::initiate_pase` reuses the PASE session the commissioner already
/// established with this peer — reuse is keyed by peer address — so each step
/// here is another exchange on that session rather than a second SPAKE2+
/// handshake. Each client view consumes its exchange, which is why there are
/// three of them: one IM transaction each.
async fn provision_network<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    peer_addr: Address,
    passcode: u32,
    credentials: &NetworkCredentials,
) -> Result<(), ApiError> {
    // Which command the device accepts is a property of the device, not of
    // what happens to be stored here, so ask rather than assume. Getting it
    // wrong produces an obscure failure several steps later.
    let features = {
        let exchange = pase_exchange(matter, crypto, peer_addr, passcode).await?;
        let raw = exchange
            .network_commissioning()
            .feature_map_read(ROOT_ENDPOINT_ID)
            .await
            .map_err(|e| {
                ApiError::commission_failed(format!(
                    "Could not read the device's NetworkCommissioning features: {:?}",
                    e.code()
                ))
            })?;
        Feature::from_bits_truncate(raw)
    };

    // The device decides, not the store: a controller may hold both kinds.
    if features.contains(Feature::WI_FI_NETWORK_INTERFACE) {
        if let Some(wifi) = &credentials.wifi {
            let exchange = pase_exchange(matter, crypto, peer_addr, passcode).await?;
            let handle = exchange
                .network_commissioning()
                .add_or_update_wi_fi_network(ROOT_ENDPOINT_ID, |req| {
                    req.ssid(OctetStr::new(wifi.ssid.as_bytes()))?
                        .credentials(OctetStr::new(wifi.password.as_bytes()))?
                        .breadcrumb(Some(0))?
                        .end()
                })
                .await
                .map_err(|e| {
                    ApiError::commission_failed(format!(
                        "AddOrUpdateWiFiNetwork failed: {:?}",
                        e.code()
                    ))
                })?;
            check_network_status(
                handle
                    .response()
                    .ok()
                    .and_then(|r| r.networking_status().ok()),
                "AddOrUpdateWiFiNetwork",
            )?;
            let _ = handle.complete().await;

            return connect_network(matter, crypto, peer_addr, passcode, wifi.ssid.as_bytes())
                .await;
        }
    }

    if features.contains(Feature::THREAD_NETWORK_INTERFACE) {
        if let Some(thread) = &credentials.thread {
            let exchange = pase_exchange(matter, crypto, peer_addr, passcode).await?;
            let handle = exchange
                .network_commissioning()
                .add_or_update_thread_network(ROOT_ENDPOINT_ID, |req| {
                    req.operational_dataset(OctetStr::new(&thread.dataset))?
                        .breadcrumb(Some(0))?
                        .end()
                })
                .await
                .map_err(|e| {
                    ApiError::commission_failed(format!(
                        "AddOrUpdateThreadNetwork failed: {:?}",
                        e.code()
                    ))
                })?;
            check_network_status(
                handle
                    .response()
                    .ok()
                    .and_then(|r| r.networking_status().ok()),
                "AddOrUpdateThreadNetwork",
            )?;
            let _ = handle.complete().await;

            return connect_network(matter, crypto, peer_addr, passcode, &thread.ext_pan_id).await;
        }
    }

    // Ethernet devices need no provisioning: they are on a network the moment
    // they are plugged in, so an empty intersection is only a failure when the
    // device actually has a radio to configure.
    if features.contains(Feature::ETHERNET_NETWORK_INTERFACE)
        && !features
            .intersects(Feature::WI_FI_NETWORK_INTERFACE | Feature::THREAD_NETWORK_INTERFACE)
    {
        return Ok(());
    }

    Err(ApiError::commission_failed(format!(
        "No usable credentials for this device. It supports {:?}, and this server has {}. \
         Store matching credentials with `set_wifi_credentials` or `set_thread_dataset`.",
        features,
        match (&credentials.wifi, &credentials.thread) {
            (Some(_), Some(_)) => "both Wi-Fi and Thread",
            (Some(_), None) => "Wi-Fi only",
            (None, Some(_)) => "Thread only",
            (None, None) => "none",
        }
    )))
}

/// Tell the device to join the network it was just given.
///
/// The device drops its Bluetooth link and brings up its radio, so this is the
/// last thing that can be said over PASE.
async fn connect_network<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    peer_addr: Address,
    passcode: u32,
    network_id: &[u8],
) -> Result<(), ApiError> {
    let exchange = pase_exchange(matter, crypto, peer_addr, passcode).await?;
    let handle = exchange
        .network_commissioning()
        .connect_network(ROOT_ENDPOINT_ID, |req| {
            req.network_id(OctetStr::new(network_id))?
                .breadcrumb(Some(0))?
                .end()
        })
        .await
        .map_err(|e| {
            ApiError::commission_failed(format!("ConnectNetwork failed: {:?}", e.code()))
        })?;
    check_network_status(
        handle
            .response()
            .ok()
            .and_then(|r| r.networking_status().ok()),
        "ConnectNetwork",
    )?;
    let _ = handle.complete().await;

    Ok(())
}

async fn pase_exchange<'a, C: Crypto>(
    matter: &'a Matter<'a>,
    crypto: &C,
    peer_addr: Address,
    passcode: u32,
) -> Result<Exchange<'a>, ApiError> {
    Exchange::initiate_pase(matter, crypto, peer_addr, passcode)
        .await
        .map_err(|e| {
            ApiError::commission_failed(format!(
                "Could not open an exchange on the PASE session: {:?}",
                e.code()
            ))
        })
}

/// Turn a `NetworkCommissioningStatus` into something an operator can act on.
///
/// These are the failures a person actually hits — a mistyped password, an
/// SSID the device cannot see — so they are worth naming rather than
/// reporting as a generic commissioning failure.
fn check_network_status(
    status: Option<NetworkCommissioningStatusEnum>,
    command: &str,
) -> Result<(), ApiError> {
    let Some(status) = status else {
        return Err(ApiError::commission_failed(format!(
            "{command} returned a response that could not be decoded"
        )));
    };

    if status == NetworkCommissioningStatusEnum::Success {
        return Ok(());
    }

    let explanation = match status {
        NetworkCommissioningStatusEnum::AuthFailure => {
            "the credentials stored on this server were rejected"
        }
        NetworkCommissioningStatusEnum::NetworkNotFound => {
            "the device cannot see that network from where it is"
        }
        NetworkCommissioningStatusEnum::NetworkIDNotFound => {
            "the device has no network configured under that id"
        }
        NetworkCommissioningStatusEnum::UnsupportedSecurity => {
            "the network's security mode is not one the device supports"
        }
        NetworkCommissioningStatusEnum::RegulatoryError => {
            "the device rejected the network on regulatory grounds"
        }
        NetworkCommissioningStatusEnum::IPV6Failed => {
            "the device joined the network but could not bring up IPv6, which Matter needs"
        }
        NetworkCommissioningStatusEnum::OutOfRange
        | NetworkCommissioningStatusEnum::BoundsExceeded => {
            "the device rejected a value as out of range"
        }
        _ => "the device rejected it",
    };

    Err(ApiError::commission_failed(format!(
        "{command} was refused: {explanation} ({status:?})"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_manual_pairing_code_into_a_short_discriminator_filter() {
        // The canonical chip-tool test code for passcode 20202021 /
        // discriminator 3840.
        let info = parse_pairing_code("34970112332").unwrap();
        assert_eq!(info.passcode, 20202021);
        // A manual code only carries the top 4 bits of the discriminator, so
        // discovery has to browse the short-discriminator subtype.
        assert_eq!(info.filter.short_discriminator, Some(0x0F));
        assert_eq!(info.filter.discriminator, None);
    }

    #[test]
    fn accepts_manual_codes_with_separators() {
        let plain = parse_pairing_code("34970112332").unwrap();
        let spaced = parse_pairing_code("3497-011-2332").unwrap();
        assert_eq!(plain.passcode, spaced.passcode);
    }

    #[test]
    fn rejects_empty_and_malformed_codes() {
        assert_eq!(parse_pairing_code("").unwrap_err().code.as_i64(), 8);
        assert_eq!(parse_pairing_code("   ").unwrap_err().code.as_i64(), 8);
        assert!(parse_pairing_code("not-a-code").is_err());
        assert!(parse_pairing_code("MT:GARBAGE").is_err());
    }
}
