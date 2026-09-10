//! TLV <-> JSON conversion.
//!
//! The reference server converts Matter values two different ways and clients
//! depend on both:
//!
//! * **Attribute reads** are *tag based*: a struct becomes a JSON object keyed
//!   by each member's TLV context tag, rendered as a decimal string.
//! * **Command (invoke) responses** are *name based*: the top-level members are
//!   keyed by their wire field names.
//!
//! Octet strings are base64 in both directions. Encoding needs the payload
//! schema for exactly that reason — a JSON string alone cannot say whether it
//! is text or bytes — so JSON is first resolved into a [`TlvNode`] tree using
//! the command metadata, and only then written. That split is also what keeps
//! argument errors reportable as `InvalidArguments` instead of surfacing as an
//! opaque TLV write failure.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{Map, Value};

use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::{TLVElement, TLVTag, TLVValue, TLVWrite};

use crate::protocol::error::ApiError;

use super::clusters::{CommandMeta, FieldKind, StructMeta};

// ---------------------------------------------------------------------------
// Decoding: TLV -> JSON
// ---------------------------------------------------------------------------

/// Decode a value the way attribute reports are reported: structs keyed by
/// context tag.
pub fn to_json(element: &TLVElement<'_>) -> Result<Value, Error> {
    decode(element, None)
}

/// Decode an invoke response: the top-level members are keyed by name, and
/// anything nested falls back to tag keys (a command's schema names only its
/// own fields).
pub fn to_json_named(
    element: &TLVElement<'_>,
    names: &BTreeMap<u32, String>,
) -> Result<Value, Error> {
    decode(element, Some(names))
}

fn decode(element: &TLVElement<'_>, names: Option<&BTreeMap<u32, String>>) -> Result<Value, Error> {
    Ok(match element.value()? {
        TLVValue::S8(v) => Value::from(v),
        TLVValue::S16(v) => Value::from(v),
        TLVValue::S32(v) => Value::from(v),
        TLVValue::S64(v) => Value::from(v),
        TLVValue::U8(v) => Value::from(v),
        TLVValue::U16(v) => Value::from(v),
        TLVValue::U32(v) => Value::from(v),
        TLVValue::U64(v) => Value::from(v),
        TLVValue::False => Value::Bool(false),
        TLVValue::True => Value::Bool(true),
        TLVValue::F32(v) => Value::from(v),
        TLVValue::F64(v) => Value::from(v),
        TLVValue::Utf8l(v) | TLVValue::Utf16l(v) | TLVValue::Utf32l(v) | TLVValue::Utf64l(v) => {
            Value::String(v.to_string())
        }
        TLVValue::Str8l(v) | TLVValue::Str16l(v) | TLVValue::Str32l(v) | TLVValue::Str64l(v) => {
            Value::String(BASE64.encode(v))
        }
        TLVValue::Null => Value::Null,
        TLVValue::Array => {
            let mut items = Vec::new();
            for child in element.array()?.iter() {
                items.push(decode(&child?, None)?);
            }
            Value::Array(items)
        }
        // Structures and lists both carry tagged members, so both decode to an
        // object; only an array is positional.
        TLVValue::Struct | TLVValue::List => {
            let mut object = Map::new();
            for (index, child) in element.container()?.iter().enumerate() {
                let child = child?;
                let key = match child.try_ctx()? {
                    Some(tag) => names
                        .and_then(|names| names.get(&(tag as u32)).cloned())
                        .unwrap_or_else(|| tag.to_string()),
                    // Anonymous members have no name to report; their position
                    // is the only stable key.
                    None => index.to_string(),
                };
                object.insert(key, decode(&child, None)?);
            }
            Value::Object(object)
        }
        TLVValue::EndCnt => Value::Null,
    })
}

// ---------------------------------------------------------------------------
// Encoding: JSON -> TLV
// ---------------------------------------------------------------------------

/// A JSON value resolved against the payload schema, ready to write.
///
/// Building this is fallible (bad base64, unknown field, non-numeric tag);
/// writing it is not, beyond running out of buffer.
#[derive(Clone, Debug, PartialEq)]
pub enum TlvNode {
    Null,
    Bool(bool),
    U64(u64),
    I64(i64),
    F64(f64),
    Utf8(String),
    Bytes(Vec<u8>),
    Struct(Vec<(u8, TlvNode)>),
    Array(Vec<TlvNode>),
}

impl TlvNode {
    pub fn write<W: TLVWrite>(&self, w: &mut W, tag: &TLVTag) -> Result<(), Error> {
        match self {
            Self::Null => w.null(tag),
            Self::Bool(value) => w.bool(tag, *value),
            Self::U64(value) => w.u64(tag, *value),
            Self::I64(value) => w.i64(tag, *value),
            Self::F64(value) => w.f64(tag, *value),
            Self::Utf8(value) => w.utf8(tag, value),
            Self::Bytes(value) => w.str(tag, value),
            Self::Struct(members) => {
                w.start_struct(tag)?;
                for (member_tag, node) in members {
                    node.write(w, &TLVTag::Context(*member_tag))?;
                }
                w.end_container()
            }
            Self::Array(items) => {
                w.start_array(tag)?;
                for item in items {
                    item.write(w, &TLVTag::Anonymous)?;
                }
                w.end_container()
            }
        }
    }
}

/// Resolve a JSON value against the kind its field is declared to hold.
///
/// An object is resolved by name when the schema says the field is a struct,
/// and by numeric TLV tag otherwise — which is how a client still reaches a
/// cluster this build has no metadata for.
pub fn from_json(value: &Value, kind: &FieldKind) -> Result<TlvNode, ApiError> {
    Ok(match value {
        Value::Null => TlvNode::Null,
        Value::Bool(value) => TlvNode::Bool(*value),
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                TlvNode::U64(value)
            } else if let Some(value) = number.as_i64() {
                TlvNode::I64(value)
            } else {
                TlvNode::F64(number.as_f64().unwrap_or_default())
            }
        }
        Value::String(text) => {
            if *kind == FieldKind::Bytes {
                TlvNode::Bytes(BASE64.decode(text).map_err(|_| {
                    ApiError::invalid_args(
                        "Expected a base64-encoded string for an octet-string field",
                    )
                })?)
            } else {
                TlvNode::Utf8(text.clone())
            }
        }
        Value::Array(items) => TlvNode::Array(
            items
                .iter()
                .map(|item| from_json(item, kind.element()))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(members) => match kind {
            FieldKind::Struct(schema) => struct_from_json(schema, members)?,
            _ => {
                let mut resolved = Vec::with_capacity(members.len());
                for (key, member) in members {
                    let tag = key.parse::<u8>().map_err(|_| {
                        ApiError::invalid_args(format!(
                            "Struct field '{}' must be addressed by its numeric TLV tag",
                            key
                        ))
                    })?;
                    resolved.push((tag, from_json(member, &FieldKind::Other)?));
                }
                resolved.sort_by_key(|(tag, _)| *tag);
                TlvNode::Struct(resolved)
            }
        },
    })
}

/// Resolve a nested struct whose fields the schema names.
///
/// A numeric key still works, so a payload written against an older build —
/// or against a field this one has no name for — keeps encoding the same way.
fn struct_from_json(schema: &StructMeta, members: &Map<String, Value>) -> Result<TlvNode, ApiError> {
    let mut resolved = Vec::with_capacity(members.len());
    for (key, member) in members {
        let tag = match schema.tag(key) {
            Some(tag) => tag,
            None => key.parse::<u32>().map_err(|_| {
                ApiError::invalid_args(format!(
                    "Unknown field '{}' for struct '{}'",
                    key, schema.name
                ))
            })?,
        };
        let tag = u8::try_from(tag).map_err(|_| {
            ApiError::invalid_args(format!("Field '{}' has an out-of-range TLV tag", key))
        })?;
        resolved.push((tag, from_json(member, schema.kind(tag as u32))?));
    }
    resolved.sort_by_key(|(tag, _)| *tag);
    Ok(TlvNode::Struct(resolved))
}

/// Resolve a `device_command` payload, whose top-level fields are named.
///
/// Unknown field names are rejected rather than dropped: silently sending a
/// command without the parameter the caller asked for is worse than an error.
pub fn command_payload_from_json(
    command: &CommandMeta,
    payload: &Value,
) -> Result<TlvNode, ApiError> {
    let members = match payload {
        Value::Null => return Ok(TlvNode::Struct(Vec::new())),
        Value::Object(members) => members,
        _ => return Err(ApiError::invalid_args("payload must be an object")),
    };

    let mut resolved = Vec::with_capacity(members.len());
    for (name, value) in members {
        let tag = match command.request_tag(name) {
            Some(tag) => tag,
            // Numeric keys address a field directly, which is how a client
            // reaches a command this build has no metadata for.
            None => name.parse::<u32>().map_err(|_| {
                ApiError::invalid_args(format!(
                    "Unknown field '{}' for command '{}'",
                    name, command.name
                ))
            })?,
        };
        let tag = u8::try_from(tag).map_err(|_| {
            ApiError::invalid_args(format!("Field '{}' has an out-of-range TLV tag", name))
        })?;
        resolved.push((tag, from_json(value, command.request_kind(tag as u32))?));
    }
    resolved.sort_by_key(|(tag, _)| *tag);
    Ok(TlvNode::Struct(resolved))
}

/// Encode an attribute value for a write. Attribute payloads are tag based, so
/// no schema is consulted; a bytes-valued attribute must be sent as base64 and
/// is detected by the caller supplying [`FieldKind::Bytes`].
pub fn attribute_value_from_json(value: &Value) -> Result<TlvNode, ApiError> {
    from_json(value, &FieldKind::Other)
}

/// Map a TLV write failure onto the protocol's SDK error.
pub fn write_error(error: Error) -> ApiError {
    if error.code() == ErrorCode::NoSpace {
        return ApiError::invalid_args("Payload is too large to encode");
    }
    ApiError::sdk(format!("Failed to encode payload: {:?}", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rs_matter::utils::storage::WriteBuf;
    use serde_json::json;

    /// Write a node and read it straight back, which is the only way to be
    /// sure the encoder and the decoder agree on the wire form.
    fn encode(node: &TlvNode, buf: &mut [u8]) -> usize {
        let mut writer = WriteBuf::new(buf);
        node.write(&mut writer, &TLVTag::Anonymous).unwrap();
        writer.get_tail()
    }

    fn roundtrip(node: &TlvNode) -> Value {
        let mut buf = [0u8; 512];
        let len = encode(node, &mut buf);
        to_json(&TLVElement::new(&buf[..len])).unwrap()
    }

    #[test]
    fn scalars_round_trip() {
        assert_eq!(roundtrip(&TlvNode::Bool(true)), json!(true));
        assert_eq!(roundtrip(&TlvNode::U64(42)), json!(42));
        assert_eq!(roundtrip(&TlvNode::I64(-7)), json!(-7));
        assert_eq!(roundtrip(&TlvNode::Utf8("ACME".into())), json!("ACME"));
        assert_eq!(roundtrip(&TlvNode::Null), Value::Null);
    }

    #[test]
    fn octet_strings_are_base64_on_the_wire() {
        let node = TlvNode::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(roundtrip(&node), json!("3q2+7w=="));
    }

    #[test]
    fn structs_are_keyed_by_tag_and_arrays_are_positional() {
        let node = TlvNode::Struct(vec![
            (0, TlvNode::U64(1)),
            (2, TlvNode::Array(vec![TlvNode::U64(7), TlvNode::U64(8)])),
        ]);
        assert_eq!(roundtrip(&node), json!({ "0": 1, "2": [7, 8] }));
    }

    #[test]
    fn nested_structs_keep_tag_keys() {
        let node = TlvNode::Struct(vec![(
            1,
            TlvNode::Struct(vec![(0, TlvNode::Utf8("inner".into()))]),
        )]);
        assert_eq!(roundtrip(&node), json!({ "1": { "0": "inner" } }));
    }

    #[test]
    fn named_decoding_applies_only_to_the_top_level() {
        let node = TlvNode::Struct(vec![
            (0, TlvNode::U64(3)),
            (1, TlvNode::Struct(vec![(0, TlvNode::Bool(true))])),
        ]);
        let mut buf = [0u8; 256];
        let len = encode(&node, &mut buf);
        let element = TLVElement::new(&buf[..len]);

        let mut names = BTreeMap::new();
        names.insert(0u32, "errorCode".to_string());
        names.insert(1u32, "details".to_string());
        assert_eq!(
            to_json_named(&element, &names).unwrap(),
            json!({ "errorCode": 3, "details": { "0": true } })
        );
    }

    #[test]
    fn command_payloads_resolve_field_names_to_tags() {
        let level = crate::matter::clusters::cluster(8).unwrap();
        let command = level.command("moveToLevelWithOnOff").unwrap();
        let payload = json!({ "level": 128, "transitionTime": 10 });
        let node = command_payload_from_json(command, &payload).unwrap();
        assert_eq!(
            node,
            TlvNode::Struct(vec![(0, TlvNode::U64(128)), (1, TlvNode::U64(10))])
        );
    }

    #[test]
    fn command_payloads_accept_numeric_tags_and_reject_unknown_names() {
        let level = crate::matter::clusters::cluster(8).unwrap();
        let command = level.command("moveToLevelWithOnOff").unwrap();
        assert!(command_payload_from_json(command, &json!({ "0": 5 })).is_ok());
        let error = command_payload_from_json(command, &json!({ "brightness": 5 })).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(error.details.contains("Unknown field 'brightness'"));
    }

    #[test]
    fn nested_payload_structs_resolve_their_field_names() {
        let door_lock = crate::matter::clusters::cluster(257).unwrap();
        let command = door_lock.command("setCredential").unwrap();
        let payload = json!({
            "operationType": 0,
            "credential": { "credentialType": 1, "credentialIndex": 2 },
            "credentialData": "3q2+7w==",
        });
        let node = command_payload_from_json(command, &payload).unwrap();
        assert_eq!(
            node,
            TlvNode::Struct(vec![
                (0, TlvNode::U64(0)),
                (
                    1,
                    TlvNode::Struct(vec![(0, TlvNode::U64(1)), (1, TlvNode::U64(2))])
                ),
                (2, TlvNode::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])),
            ])
        );
    }

    #[test]
    fn nested_structs_still_accept_numeric_tags() {
        let door_lock = crate::matter::clusters::cluster(257).unwrap();
        let command = door_lock.command("setCredential").unwrap();
        let named = json!({ "credential": { "credentialType": 1, "credentialIndex": 2 } });
        let numeric = json!({ "1": { "0": 1, "1": 2 } });
        assert_eq!(
            command_payload_from_json(command, &named).unwrap(),
            command_payload_from_json(command, &numeric).unwrap()
        );
    }

    #[test]
    fn an_unknown_nested_field_names_the_struct_it_was_meant_for() {
        let door_lock = crate::matter::clusters::cluster(257).unwrap();
        let command = door_lock.command("setCredential").unwrap();
        let payload = json!({ "credential": { "kind": 1 } });
        let error = command_payload_from_json(command, &payload).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(
            error.details.contains("Unknown field 'kind'")
                && error.details.contains("credentialStruct"),
            "{}",
            error.details
        );
    }

    #[test]
    fn a_list_of_structs_resolves_every_element() {
        let content_control = crate::matter::clusters::cluster(1295).unwrap();
        let command = content_control.command("addBlockApplications").unwrap();
        let payload = json!({
            "applications": [
                { "catalogVendorID": 1, "applicationID": "one" },
                { "catalogVendorID": 2, "applicationID": "two" },
            ]
        });
        let node = command_payload_from_json(command, &payload).unwrap();
        assert_eq!(
            node,
            TlvNode::Struct(vec![(
                0,
                TlvNode::Array(vec![
                    TlvNode::Struct(vec![
                        (0, TlvNode::U64(1)),
                        (1, TlvNode::Utf8("one".into()))
                    ]),
                    TlvNode::Struct(vec![
                        (0, TlvNode::U64(2)),
                        (1, TlvNode::Utf8("two".into()))
                    ]),
                ])
            )])
        );
    }

    #[test]
    fn octet_string_payload_fields_are_base64_decoded() {
        let network = crate::matter::clusters::cluster(49).unwrap();
        let command = network.command("addOrUpdateThreadNetwork").unwrap();
        let node = command_payload_from_json(command, &json!({ "operationalDataset": "3q2+7w==" }))
            .unwrap();
        assert_eq!(
            node,
            TlvNode::Struct(vec![(0, TlvNode::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]))])
        );

        let error =
            command_payload_from_json(command, &json!({ "operationalDataset": "not base64!" }))
                .unwrap_err();
        assert!(error.details.contains("base64"));
    }

    #[test]
    fn an_empty_payload_encodes_as_an_empty_struct() {
        let on_off = crate::matter::clusters::cluster(6).unwrap();
        let command = on_off.command("toggle").unwrap();
        assert_eq!(
            command_payload_from_json(command, &Value::Null).unwrap(),
            TlvNode::Struct(Vec::new())
        );
        assert_eq!(
            command_payload_from_json(command, &json!({})).unwrap(),
            TlvNode::Struct(Vec::new())
        );
    }

    #[test]
    fn attribute_writes_address_struct_fields_by_tag() {
        let node = attribute_value_from_json(&json!({ "1": 5, "0": "text" })).unwrap();
        assert_eq!(
            node,
            TlvNode::Struct(vec![
                (0, TlvNode::Utf8("text".into())),
                (1, TlvNode::U64(5))
            ])
        );
        let error = attribute_value_from_json(&json!({ "label": 5 })).unwrap_err();
        assert!(error.details.contains("numeric TLV tag"));
    }
}
