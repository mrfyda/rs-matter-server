//! Minimal Thread operational dataset decoder.
//!
//! A dataset is a flat MeshCoP TLV sequence: one type byte, one length byte,
//! then the value. Only the fields the protocol surfaces are decoded — the
//! network name and extended PAN id for credential summaries, and whether the
//! PSKc and network key are present, which is what decides if MeshCoP
//! diagnostics can be attempted for that network.

/// MeshCoP TLV types used here.
const TLV_EXT_PAN_ID: u8 = 2;
const TLV_NETWORK_NAME: u8 = 3;
const TLV_PSKC: u8 = 4;
const TLV_NETWORK_KEY: u8 = 5;

/// A length byte of 0xFF introduces a 16-bit extended length.
const EXTENDED_LENGTH_MARKER: u8 = 0xFF;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationalDataset {
    pub network_name: Option<String>,
    /// Uppercase hex, as the reference reports it.
    pub ext_pan_id: Option<String>,
    pub has_pskc: bool,
    pub has_network_key: bool,
}

impl OperationalDataset {
    /// MeshCoP diagnostics need both secrets to petition the Border Router.
    pub fn supports_meshcop(&self) -> bool {
        self.has_pskc && self.has_network_key
    }

    /// The extended PAN id as raw bytes.
    ///
    /// `ConnectNetwork` identifies a Thread network by these bytes, where a
    /// Wi-Fi network is identified by its SSID. Kept here so hex handling
    /// stays in one place.
    pub fn ext_pan_id_bytes(&self) -> Option<Vec<u8>> {
        self.ext_pan_id.as_deref().and_then(from_hex)
    }
}

/// Parse a hex-encoded dataset. Returns `None` when the hex or the TLV framing
/// is malformed; callers degrade to an id-only summary rather than failing the
/// request, matching the reference.
pub fn decode(hex_dataset: &str) -> Option<OperationalDataset> {
    let bytes = from_hex(hex_dataset)?;
    let mut dataset = OperationalDataset::default();
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        let tlv_type = bytes[cursor];
        let length_byte = *bytes.get(cursor + 1)?;
        let (length, header) = if length_byte == EXTENDED_LENGTH_MARKER {
            let high = *bytes.get(cursor + 2)? as usize;
            let low = *bytes.get(cursor + 3)? as usize;
            ((high << 8) | low, 4)
        } else {
            (length_byte as usize, 2)
        };
        let start = cursor + header;
        let end = start.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        let value = &bytes[start..end];

        match tlv_type {
            TLV_NETWORK_NAME => {
                dataset.network_name = std::str::from_utf8(value).ok().map(str::to_string)
            }
            TLV_EXT_PAN_ID => dataset.ext_pan_id = Some(to_hex_upper(value)),
            TLV_PSKC => dataset.has_pskc = !value.iter().all(|&b| b == 0),
            TLV_NETWORK_KEY => dataset.has_network_key = !value.iter().all(|&b| b == 0),
            _ => {}
        }
        cursor = end;
    }

    Some(dataset)
}

/// Validate the wire form of a dataset argument without decoding it.
pub fn is_valid_hex(dataset: &str) -> bool {
    !dataset.is_empty()
        && dataset.len().is_multiple_of(2)
        && dataset.chars().all(|c| c.is_ascii_hexdigit())
}

/// Decode a hex string. Public because commissioning needs the dataset as raw
/// bytes to hand to `AddOrUpdateThreadNetwork`.
pub fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !is_valid_hex(hex) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

fn to_hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02X}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network name "MyThreadNet", ext PAN id 1122334455667788, plus a
    /// non-zero PSKc and network key.
    fn sample() -> String {
        let name = "MyThreadNet";
        let name_hex: String = name.bytes().map(|b| format!("{:02x}", b)).collect();
        format!(
            "0208{}03{:02x}{}04{:02x}{}05{:02x}{}",
            "1122334455667788",
            name.len(),
            name_hex,
            16,
            "000102030405060708090a0b0c0d0e0f",
            16,
            "0f0e0d0c0b0a09080706050403020100"
        )
    }

    #[test]
    fn decodes_name_and_ext_pan_id() {
        let dataset = decode(&sample()).unwrap();
        assert_eq!(dataset.network_name.as_deref(), Some("MyThreadNet"));
        assert_eq!(dataset.ext_pan_id.as_deref(), Some("1122334455667788"));
        assert!(dataset.supports_meshcop());
    }

    #[test]
    fn all_zero_secrets_do_not_enable_meshcop() {
        let dataset =
            decode("0410000000000000000000000000000000000510ffffffffffffffffffffffffffffffff")
                .unwrap();
        assert!(!dataset.has_pskc);
        assert!(dataset.has_network_key);
        assert!(!dataset.supports_meshcop());
    }

    #[test]
    fn malformed_datasets_decode_to_none() {
        assert!(decode("zz").is_none());
        assert!(decode("0208112233").is_none(), "truncated value");
        assert!(decode("020").is_none(), "odd length");
    }

    #[test]
    fn hex_validation_matches_the_documented_rule() {
        assert!(is_valid_hex("00ff"));
        assert!(!is_valid_hex(""));
        assert!(!is_valid_hex("abc"));
        assert!(!is_valid_hex("zz"));
    }
}
