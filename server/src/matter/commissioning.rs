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
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::onboard::{CommissionOptions, Commissioner};
use rs_matter::pairing::qr::QrPayload;
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
    icac_private_key: &CanonPkcSecretKey,
    peer_addr: Address,
    passcode: u32,
    node_id: u64,
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
            NocGenerator::create(
                icac_private_key.reference(),
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
    commissioner
        .complete_via_case(peer_addr, &phase1)
        .await
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
