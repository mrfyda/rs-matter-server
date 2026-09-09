//! The value encoding matter.js uses for everything it persists.
//!
//! matter.js stores JSON, but JSON cannot hold the two types Matter state is
//! full of — 64-bit integers and byte strings — so its `toJson` writes those
//! as a *string* containing a small tagged object:
//!
//! ```text
//! {"__object__":"Uint8Array","__value__":"1e2f.."}
//! {"__object__":"BigInt","__value__":"1152921504606846977"}
//! ```
//!
//! The double encoding is not an accident of ours to undo: `JSON.stringify`'s
//! replacer can only return a value, so matter.js returns a string and parses
//! it back on the way in. Every driver — `wal`, `file`, `json` — writes values
//! this way, so this module is the one place that has to know about it.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

/// A decoded matter.js storage value.
///
/// `Undefined` decodes to [`MjValue::Null`]: matter.js distinguishes the two,
/// but nothing this crate reads treats an absent value differently from a null
/// one, and collapsing them keeps every caller from having to.
#[derive(Clone, Debug, PartialEq)]
pub enum MjValue {
    Null,
    Bool(bool),
    /// A JSON number. Matter ids that fit in a `f64` arrive here.
    Number(f64),
    /// A value matter.js wrote as a `BigInt`, `NodeId`, `FabricId` or
    /// `EventNumber` — kept exact, because node ids routinely exceed `f64`'s
    /// integer range.
    BigInt(i128),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<MjValue>),
    Object(BTreeMap<String, MjValue>),
}

const TYPE_KEY: &str = "__object__";
const VALUE_KEY: &str = "__value__";

impl MjValue {
    /// Decode one value from the JSON text a driver stored.
    pub fn from_json_str(text: &str) -> Result<Self> {
        let raw: serde_json::Value =
            serde_json::from_str(text).context("parsing a matter.js storage value")?;
        Ok(Self::from_json(raw))
    }

    /// Decode an already-parsed JSON value, resolving the tagged strings.
    ///
    /// Decoding never fails. A store holds far more than an import reads —
    /// every cached attribute of every node — and a tag added by a future
    /// matter.js in some cluster's state must not be able to stop a migration.
    /// Anything unrecognised stays as the text it was written as, where the
    /// typed accessors will reject it if it turns out to be a value that
    /// actually mattered.
    pub fn from_json(raw: serde_json::Value) -> Self {
        match raw {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(number) => Self::number(&number),
            serde_json::Value::String(text) => Self::from_string(text),
            serde_json::Value::Array(items) => {
                Self::Array(items.into_iter().map(Self::from_json).collect())
            }
            serde_json::Value::Object(fields) => Self::Object(
                fields
                    .into_iter()
                    .map(|(name, value)| (name, Self::from_json(value)))
                    .collect(),
            ),
        }
    }

    /// A JSON number, kept exact when it is an integer.
    ///
    /// Going through `f64` unconditionally would quietly round any value above
    /// 2^53, and the values here are identifiers where a rounded copy is a
    /// different device.
    fn number(number: &serde_json::Number) -> Self {
        if let Some(value) = number.as_u64() {
            return Self::BigInt(value as i128);
        }
        if let Some(value) = number.as_i64() {
            return Self::BigInt(value as i128);
        }
        Self::Number(number.as_f64().unwrap_or_default())
    }

    /// A stored string is either a plain string or one of the tagged objects.
    fn from_string(text: String) -> Self {
        // Cheap rejection first: the overwhelming majority of strings are
        // ordinary text, and parsing every one of them as JSON to find out
        // would dominate the cost of reading a large store.
        if !text.starts_with(&format!("{{\"{}\":\"", TYPE_KEY)) || !text.ends_with('}') {
            return Self::String(text);
        }

        // A string that merely looks like the encoding is still a string.
        let Ok(tagged) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::String(text);
        };
        let Some(kind) = tagged.get(TYPE_KEY).and_then(serde_json::Value::as_str) else {
            return Self::String(text);
        };
        let value = tagged.get(VALUE_KEY);

        let decoded = match kind {
            "Undefined" => Some(Self::Null),
            "BigInt" | "NodeId" | "FabricId" | "EventNumber" => value
                .and_then(number_like)
                .and_then(|digits| digits.parse::<i128>().ok())
                .map(Self::BigInt),
            "Uint8Array" => value
                .and_then(serde_json::Value::as_str)
                .and_then(decode_hex)
                .map(Self::Bytes),
            // A map's entries are themselves an encoded JSON document, one
            // level deeper than everything else.
            "Map" => value
                .and_then(serde_json::Value::as_str)
                .and_then(|inner| Self::from_json_str(inner).ok()),
            // Matter's newtype ids, written by older versions of matter.js.
            // The wide ones are handled by the `BigInt` arm above.
            "AttributeId"
            | "CaseAuthenticatedTag"
            | "ClusterId"
            | "CommandId"
            | "DataVersion"
            | "DeviceTypeId"
            | "EndpointNumber"
            | "EntryIndex"
            | "EventId"
            | "FabricIndex"
            | "FieldId"
            | "GroupId"
            | "VendorId"
            | "Interval" => {
                value.and_then(number_like).and_then(|digits| {
                    digits
                        .parse::<i128>()
                        .map(Self::BigInt)
                        // Not every legacy tag carried an integer.
                        .or_else(|_| digits.parse::<f64>().map(Self::Number))
                        .ok()
                })
            }
            _ => None,
        };

        decoded.unwrap_or(Self::String(text))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Every integral spelling matter.js might have used for one number.
    ///
    /// A node id may be stored as a `BigInt` on one install and a plain number
    /// on another — matter.js narrows to `number` whenever the value fits —
    /// so readers must accept both or break on half the fabrics in the field.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) if *value >= 0.0 && value.fract() == 0.0 => Some(*value as u64),
            Self::BigInt(value) if *value >= 0 => u64::try_from(*value).ok(),
            Self::String(text) => text.parse().ok(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[MjValue]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Field lookup, for the object values (`Fabric.Config`, `PeerAddress`).
    pub fn get(&self, field: &str) -> Option<&MjValue> {
        match self {
            Self::Object(fields) => fields.get(field).filter(|value| **value != Self::Null),
            _ => None,
        }
    }
}

/// A tagged value's payload, whether it was written as a string or a number.
fn number_like(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// `None` for anything that is not an even-length run of hex digits, which
/// leaves the value as the text it was stored as.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            (pair.len() == 2)
                .then(|| u8::from_str_radix(pair, 16).ok())
                .flatten()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_strings_decode_from_the_tagged_form() {
        let value =
            MjValue::from_json_str(r#""{\"__object__\":\"Uint8Array\",\"__value__\":\"00ff10\"}""#)
                .unwrap();
        assert_eq!(value.as_bytes(), Some(&[0x00, 0xff, 0x10][..]));
    }

    #[test]
    fn node_ids_survive_beyond_the_range_of_a_double() {
        // 2^53 + 1: the first integer a JSON number cannot represent exactly,
        // and well within the range of a Matter node id.
        let value = MjValue::from_json_str(
            r#""{\"__object__\":\"BigInt\",\"__value__\":\"9007199254740993\"}""#,
        )
        .unwrap();
        assert_eq!(value.as_u64(), Some(9007199254740993));
    }

    #[test]
    fn a_number_and_a_bigint_read_as_the_same_id() {
        assert_eq!(MjValue::from_json_str("5").unwrap().as_u64(), Some(5));
        assert_eq!(
            MjValue::from_json_str(r#""{\"__object__\":\"NodeId\",\"__value__\":\"5\"}""#)
                .unwrap()
                .as_u64(),
            Some(5)
        );
    }

    #[test]
    fn a_plain_json_integer_keeps_every_bit() {
        // Written as a number rather than a tagged BigInt, and larger than a
        // double can hold exactly.
        let value = MjValue::from_json_str("1234605616436508552").unwrap();
        assert_eq!(value.as_u64(), Some(1_234_605_616_436_508_552));
    }

    #[test]
    fn ordinary_strings_are_left_alone() {
        let value = MjValue::from_json_str(r#""HomeAssistant""#).unwrap();
        assert_eq!(value.as_str(), Some("HomeAssistant"));

        // Text that starts like the encoding but is not it.
        let value = MjValue::from_json_str(r#""{\"__object__\":\"but not json}""#).unwrap();
        assert_eq!(value.as_str(), Some("{\"__object__\":\"but not json}"));
    }

    #[test]
    fn objects_expose_their_fields_and_hide_undefined_ones() {
        let value = MjValue::from_json_str(
            r#"{"label":"Home","intermediateCACert":"{\"__object__\":\"Undefined\"}","fabricIndex":1}"#,
        )
        .unwrap();
        assert_eq!(value.get("label").and_then(MjValue::as_str), Some("Home"));
        assert_eq!(value.get("fabricIndex").and_then(MjValue::as_u64), Some(1));
        assert!(
            value.get("intermediateCACert").is_none(),
            "an undefined field must read as absent, not as a value"
        );
    }

    #[test]
    fn maps_decode_through_their_extra_layer() {
        // toJson writes a Map as a string holding the JSON of its entries.
        let value = MjValue::from_json_str(
            r#""{\"__object__\":\"Map\",\"__value__\":\"[[\\\"a\\\",1]]\"}""#,
        )
        .unwrap();
        let entries = value.as_array().expect("a map decodes to its entry list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].as_array().unwrap()[0].as_str(), Some("a"));
    }

    #[test]
    fn a_truncated_byte_string_is_not_read_as_a_short_one() {
        let value =
            MjValue::from_json_str(r#""{\"__object__\":\"Uint8Array\",\"__value__\":\"abc\"}""#)
                .unwrap();
        assert_eq!(
            value.as_bytes(),
            None,
            "half a byte must not decode as a byte"
        );
    }

    #[test]
    fn an_unknown_tag_survives_as_text_instead_of_stopping_the_import() {
        // A type a future matter.js adds, in some cluster's cached state. The
        // values an import actually needs are checked where they are read.
        let value =
            MjValue::from_json_str(r#""{\"__object__\":\"Quaternion\",\"__value__\":\"1\"}""#)
                .unwrap();
        assert!(value.as_str().unwrap().contains("Quaternion"));
        assert_eq!(value.as_bytes(), None);
        assert_eq!(value.as_u64(), None);
    }
}
