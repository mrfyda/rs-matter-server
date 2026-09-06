//! Field-name transform shared with the reference server.
//!
//! Matter spec names are PascalCase; the wire uses the chip-SDK spelling,
//! which preserves a fixed set of acronyms and then lowercases the first
//! letter unless the name opens with one. Clients index command payloads and
//! invoke responses by these names, so the transform has to agree with the
//! reference exactly — it is ported from `@matter-server/ws-client`'s
//! `wire-naming.ts` rather than approximated.

/// Acronyms the chip SDK keeps uppercase.
///
/// Order matters: entries are applied left to right, so an acronym that is a
/// suffix of a longer one must come after it (`SNTP` before `NTP`, `UTC`
/// before `TC`, `PIN` before `PI`), otherwise the short form matches first and
/// leaves the remainder unexpanded.
const ACRONYMS: &[&str] = &[
    "SNTPNTS", "NTPNTS", "BLEUWB", "HVAC", "ICAC", "DAC", "MAC", "EVSE", "RFID", "PIR", "IPV",
    "ANSI", "BDX", "BLE", "CEC", "CO", "CSR", "DNS", "DST", "ESA", "EV", "GHG", "ICD", "IEC", "IP",
    "LED", "MLE", "NFC", "NOC", "SNTP", "NTP", "OTA", "PAI", "PHY", "PIN", "PI", "PV", "PAKE",
    "IPK", "LQI", "URI", "RF", "RMS", "UTC", "TC", "URL", "UWB", "VID", "AC", "ID",
];

/// Names whose wire spelling the chip SDK fixed by hand. Keys are the spec
/// name; values are the final wire name, so no further transform applies.
const FIELD_NAME_OVERRIDES: &[(&str, &str)] = &[
    ("Id", "id"),
    ("PanId", "panId"),
    ("ExtendedPanId", "extendedPanId"),
    ("LeaderRouterId", "leaderRouterId"),
    ("PartitionId", "partitionId"),
    ("PartitionIdChangeCount", "partitionIdChangeCount"),
    ("RouterId", "routerId"),
    ("ExtendedPanIdPresent", "extendedPanIdPresent"),
    ("PanIdPresent", "panIdPresent"),
    ("AdminVendorId", "adminVendorId"),
    ("Icac", "icac"),
    ("Noc", "noc"),
    ("MleFrameCounter", "mleFrameCounter"),
    ("DvbiUrl", "dvbiUrl"),
    ("PosterArtUrl", "posterArtUrl"),
    ("ThumbnailUrl", "thumbnailUrl"),
    ("CommissioningArl", "commissioningARL"),
    ("ArlRequestFlowUrl", "ARLRequestFlowUrl"),
    ("Lqi", "lqi"),
    ("RootCaCertificate", "rootCACertificate"),
    ("Watermark", "waterMark"),
    ("NocsrElements", "NOCSRElements"),
    ("AcVoltageMultiplier", "acVoltageMultiplier"),
    ("AcVoltageDivisor", "acVoltageDivisor"),
    ("AcCurrentMultiplier", "acCurrentMultiplier"),
    ("AcCurrentDivisor", "acCurrentDivisor"),
    ("AcPowerMultiplier", "acPowerMultiplier"),
    ("AcPowerDivisor", "acPowerDivisor"),
    (
        "RequirePinForRemoteOperation",
        "requirePINforRemoteOperation",
    ),
    ("AcCapacityFormat", "ACCapacityformat"),
    ("IPv4Addresses", "IPv4Addresses"),
    ("IPv6Addresses", "IPv6Addresses"),
    (
        "OffPremiseServicesReachableIPv4",
        "offPremiseServicesReachableIPv4",
    ),
    (
        "OffPremiseServicesReachableIPv6",
        "offPremiseServicesReachableIPv6",
    ),
    ("ColorPointBx", "colorPointBX"),
    ("ColorPointBy", "colorPointBY"),
    ("ColorPointGx", "colorPointGX"),
    ("ColorPointGy", "colorPointGY"),
    ("ColorPointRx", "colorPointRX"),
    ("ColorPointRy", "colorPointRY"),
];

/// True when an acronym occurrence ends at a word boundary: the next character
/// is uppercase, absent, or non-alphabetic — or a plural `s` that is itself at
/// such a boundary.
fn ends_word(rest: &str) -> bool {
    let mut chars = rest.chars();
    match chars.next() {
        None => true,
        Some(c) if c.is_ascii_uppercase() => true,
        Some(c) if !c.is_ascii_alphabetic() => true,
        Some('s') => {
            let after = chars.next();
            match after {
                None => true,
                Some(c) if c.is_ascii_uppercase() || !c.is_ascii_alphabetic() => true,
                _ => false,
            }
        }
        _ => false,
    }
}

/// Expand TitleCase acronyms (`PinCode` -> `PINCode`), leaving names that are
/// already spelled with the acronym untouched.
pub fn to_chip_name(name: &str) -> String {
    let mut result = name.to_string();
    for acronym in ACRONYMS {
        let mut title = String::with_capacity(acronym.len());
        let mut chars = acronym.chars();
        if let Some(first) = chars.next() {
            title.push(first);
            title.extend(chars.map(|c| c.to_ascii_lowercase()));
        }
        if title == *acronym {
            continue;
        }
        let mut out = String::with_capacity(result.len());
        let mut rest = result.as_str();
        while let Some(index) = rest.find(&title) {
            let after = &rest[index + title.len()..];
            out.push_str(&rest[..index]);
            if ends_word(after) {
                out.push_str(acronym);
            } else {
                out.push_str(&title);
            }
            rest = after;
        }
        out.push_str(rest);
        result = out;
    }
    result
}

/// Convert a spec name to its wire field name.
pub fn wire_field_name(name: &str) -> String {
    if let Some((_, override_name)) = FIELD_NAME_OVERRIDES.iter().find(|(key, _)| *key == name) {
        return (*override_name).to_string();
    }
    let chip_name = to_chip_name(name);
    let mut chars = chip_name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let opens_with_acronym = first.is_ascii_uppercase()
        && chip_name
            .chars()
            .nth(1)
            .map(|second| second.is_ascii_uppercase())
            .unwrap_or(false);
    if opens_with_acronym {
        return chip_name;
    }
    first.to_ascii_lowercase().to_string() + chars.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_title_cased_acronyms_at_word_boundaries() {
        assert_eq!(to_chip_name("PinCode"), "PINCode");
        assert_eq!(to_chip_name("IcacValue"), "ICACValue");
        assert_eq!(to_chip_name("ProductUrl"), "ProductURL");
        assert_eq!(to_chip_name("VendorId"), "VendorID");
    }

    #[test]
    fn leaves_mid_word_matches_alone() {
        // "Pin" inside "Pinning" is not an acronym occurrence.
        assert_eq!(to_chip_name("PinningMode"), "PinningMode");
        // A plural at a boundary still expands.
        assert_eq!(to_chip_name("Nocs"), "NOCs");
    }

    #[test]
    fn names_already_spelled_with_acronyms_are_unchanged() {
        assert_eq!(to_chip_name("AddNOC"), "AddNOC");
        assert_eq!(to_chip_name("CSRRequest"), "CSRRequest");
        assert_eq!(
            to_chip_name("RequirePINforRemoteOperation"),
            "RequirePINforRemoteOperation"
        );
    }

    #[test]
    fn wire_names_lowercase_only_non_acronym_openings() {
        assert_eq!(wire_field_name("Level"), "level");
        assert_eq!(wire_field_name("TransitionTime"), "transitionTime");
        assert_eq!(wire_field_name("PinCode"), "PINCode");
        assert_eq!(wire_field_name("CSRRequest"), "CSRRequest");
        assert_eq!(wire_field_name("NOCs"), "NOCs");
        assert_eq!(wire_field_name("ProductURL"), "productURL");
    }

    #[test]
    fn overrides_win_over_the_generic_transform() {
        // Without the override the Id acronym would produce "adminVendorID".
        assert_eq!(wire_field_name("AdminVendorId"), "adminVendorId");
        assert_eq!(wire_field_name("Lqi"), "lqi");
        assert_eq!(wire_field_name("Icac"), "icac");
        assert_eq!(wire_field_name("IPv6Addresses"), "IPv6Addresses");
    }

    #[test]
    fn empty_names_are_handled() {
        assert_eq!(wire_field_name(""), "");
    }
}
