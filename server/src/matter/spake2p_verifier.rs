//! SPAKE2+ passcode verifier computation.
//!
//! `open_commissioning_window` opens an *enhanced* window, which hands the
//! device a PAKE verifier rather than a passcode: the controller picks a fresh
//! passcode, derives `w0 || L` from it, and sends only the derived value, so
//! the passcode itself never crosses the wire. rs-matter derives this
//! internally for its own PASE responder but does not expose it, so it is
//! computed here from the same `Crypto` primitives.
//!
//! Per the Matter spec: `w0s || w1s = PBKDF2(passcode_le, salt, iterations)`
//! over 80 bytes, `w0 = w0s mod p`, `w1 = w1s mod p`, `L = w1 * G`, and the
//! verifier is `w0 (32 bytes) || L (65 bytes, uncompressed)`.

use rs_matter::crypto::{Crypto, CryptoSensitive, CryptoSensitiveRef, EcPoint, EcScalar, PbKdf};

use crate::protocol::error::ApiError;

/// Passcode length in bytes (a little-endian `u32`).
const PASSCODE_LEN: usize = 4;
/// PBKDF2 output: two 40-byte values.
const W_LEN: usize = 80;
const W_HALF_LEN: usize = 40;
/// Canonical P-256 scalar and uncompressed point lengths.
const SCALAR_LEN: usize = 32;
const POINT_LEN: usize = 65;
/// Total verifier length.
pub const VERIFIER_LEN: usize = SCALAR_LEN + POINT_LEN;

/// The spec's minimum, and what the reference uses.
pub const DEFAULT_ITERATIONS: u32 = 1000;
/// The spec's salt length bounds.
pub const MIN_SALT_LEN: usize = 16;
pub const MAX_SALT_LEN: usize = 32;

/// Compute `w0 || L` for a passcode.
pub fn compute_verifier<C: Crypto>(
    crypto: &C,
    passcode: u32,
    salt: &[u8],
    iterations: u32,
) -> Result<Vec<u8>, ApiError> {
    if !(MIN_SALT_LEN..=MAX_SALT_LEN).contains(&salt.len()) {
        return Err(ApiError::invalid_args(format!(
            "PAKE salt must be {}..={} bytes",
            MIN_SALT_LEN, MAX_SALT_LEN
        )));
    }
    if iterations < DEFAULT_ITERATIONS {
        return Err(ApiError::invalid_args(format!(
            "PAKE iteration count must be at least {}",
            DEFAULT_ITERATIONS
        )));
    }

    let passcode_bytes = passcode.to_le_bytes();
    let password = CryptoSensitiveRef::<PASSCODE_LEN>::new(&passcode_bytes);

    let mut w = CryptoSensitive::<W_LEN>::new();
    crypto
        .pbkdf()
        .map_err(|e| ApiError::sdk(format!("PBKDF2 unavailable: {:?}", e.code())))?
        .derive(password, iterations as usize, salt, &mut w)
        .map_err(|e| ApiError::sdk(format!("PBKDF2 failed: {:?}", e.code())))?;

    let (w0s, w1s) = w.reference().split::<W_HALF_LEN, W_HALF_LEN>();

    let w0 = crypto
        .ec_scalar_mod_p(w0s)
        .map_err(|e| ApiError::sdk(format!("w0 reduction failed: {:?}", e.code())))?;
    let w1 = crypto
        .ec_scalar_mod_p(w1s)
        .map_err(|e| ApiError::sdk(format!("w1 reduction failed: {:?}", e.code())))?;

    let l_point = crypto
        .ec_generator_point()
        .map_err(|e| ApiError::sdk(format!("generator unavailable: {:?}", e.code())))?
        .mul(&w1)
        .map_err(|e| ApiError::sdk(format!("L computation failed: {:?}", e.code())))?;

    let mut w0_canon = CryptoSensitive::<SCALAR_LEN>::new();
    w0.write_canon(&mut w0_canon)
        .map_err(|e| ApiError::sdk(format!("w0 encoding failed: {:?}", e.code())))?;
    let mut l_canon = CryptoSensitive::<POINT_LEN>::new();
    l_point
        .write_canon(&mut l_canon)
        .map_err(|e| ApiError::sdk(format!("L encoding failed: {:?}", e.code())))?;

    let mut verifier = Vec::with_capacity(VERIFIER_LEN);
    verifier.extend_from_slice(w0_canon.access());
    verifier.extend_from_slice(l_canon.access());
    Ok(verifier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use rs_matter::crypto::test_only_crypto;

    /// The salt the Matter test vectors use: the ASCII string
    /// "SPAKE2P Key Salt".
    const TEST_SALT: &[u8] = b"SPAKE2P Key Salt";

    #[test]
    fn matches_the_matter_test_vector() {
        let crypto = test_only_crypto();
        let verifier = compute_verifier(&crypto, 20202021, TEST_SALT, 1000).unwrap();
        assert_eq!(
            BASE64.encode(&verifier),
            "uWFwqugDNGiEck/po7KHwwMwwqZgN10XuyBajPGuyzUEV/iree4lOrao5GuwnlQ65CJzbeUB49s31EH+NEkg0JVI5MGCQGMMT/SRPFNRODm3wH/MBiehuFc6FJ/NH6Rmzw=="
        );
    }

    #[test]
    fn verifier_has_the_canonical_length_and_an_uncompressed_point() {
        let crypto = test_only_crypto();
        let verifier = compute_verifier(&crypto, 12345678, TEST_SALT, 1000).unwrap();
        assert_eq!(verifier.len(), VERIFIER_LEN);
        // L is an uncompressed P-256 point.
        assert_eq!(verifier[SCALAR_LEN], 0x04);
    }

    #[test]
    fn different_passcodes_derive_different_verifiers() {
        let crypto = test_only_crypto();
        let a = compute_verifier(&crypto, 20202021, TEST_SALT, 1000).unwrap();
        let b = compute_verifier(&crypto, 20202022, TEST_SALT, 1000).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn out_of_range_parameters_are_rejected() {
        let crypto = test_only_crypto();
        assert!(compute_verifier(&crypto, 1, b"short", 1000).is_err());
        assert!(compute_verifier(&crypto, 1, TEST_SALT, 999).is_err());
    }
}
