//! Request/response envelopes and typed argument access.
//!
//! Handlers never touch the envelope: they receive [`Args`] and return an
//! [`ApiResult`], and the dispatcher renders whichever of the two wire shapes
//! applies. Keeping that split here is what makes the command handlers
//! testable without a socket.

use serde_json::{json, Value};

use super::error::{ApiError, ApiResult};

/// A decoded client request.
#[derive(Clone, Debug)]
pub struct Request {
    pub message_id: Value,
    pub command: Option<String>,
    pub args: Args,
}

impl Request {
    /// Decode a raw client frame. A frame that is not an object, or carries no
    /// `command`, still produces a `Request` so the dispatcher can answer with
    /// the documented `InvalidCommand` error rather than dropping the frame.
    pub fn from_value(raw: &Value) -> Self {
        let message_id = raw
            .get("message_id")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        let command = raw
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string);
        let args = Args::new(raw.get("args").cloned().unwrap_or(Value::Null));
        Self {
            message_id,
            command,
            args,
        }
    }
}

/// Render a handler result into the wire envelope.
pub fn response_envelope(message_id: &Value, result: ApiResult) -> Value {
    match result {
        Ok(result) => json!({ "message_id": message_id, "result": result }),
        Err(error) => json!({
            "message_id": message_id,
            "error_code": error.code.as_i64(),
            "details": error.details,
        }),
    }
}

/// Command arguments, with accessors that produce the documented
/// `InvalidArguments` error instead of silently defaulting.
///
/// `args` is optional on the wire, so a missing object and an empty object
/// behave identically; only a *required* field's absence is an error.
#[derive(Clone, Debug, Default)]
pub struct Args(Value);

impl Args {
    pub fn new(value: Value) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &Value {
        &self.0
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        match self.0.get(name) {
            Some(Value::Null) | None => None,
            Some(value) => Some(value),
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.0 {
            Value::Null => true,
            Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    /// Node IDs and other 64-bit identifiers. Accepts a JSON number or a
    /// decimal string: clients that cannot represent 64-bit integers natively
    /// send the latter.
    pub fn u64(&self, name: &str) -> Result<Option<u64>, ApiError> {
        let Some(value) = self.get(name) else {
            return Ok(None);
        };
        match value {
            Value::Number(number) => number
                .as_u64()
                .ok_or_else(|| {
                    ApiError::invalid_args(format!("{} must be a positive integer", name))
                })
                .map(Some),
            Value::String(text) => text
                .parse::<u64>()
                .map_err(|_| ApiError::invalid_args(format!("{} must be a positive integer", name)))
                .map(Some),
            _ => Err(ApiError::invalid_args(format!(
                "{} must be a positive integer",
                name
            ))),
        }
    }

    pub fn req_u64(&self, name: &str) -> Result<u64, ApiError> {
        self.u64(name)?
            .ok_or_else(|| ApiError::invalid_args(format!("Missing {}", name)))
    }

    pub fn u32(&self, name: &str) -> Result<Option<u32>, ApiError> {
        self.bounded(name, u32::MAX as u64)
            .map(|v| v.map(|v| v as u32))
    }

    pub fn req_u32(&self, name: &str) -> Result<u32, ApiError> {
        self.u32(name)?
            .ok_or_else(|| ApiError::invalid_args(format!("Missing {}", name)))
    }

    pub fn u16(&self, name: &str) -> Result<Option<u16>, ApiError> {
        self.bounded(name, u16::MAX as u64)
            .map(|v| v.map(|v| v as u16))
    }

    pub fn req_u16(&self, name: &str) -> Result<u16, ApiError> {
        self.u16(name)?
            .ok_or_else(|| ApiError::invalid_args(format!("Missing {}", name)))
    }

    pub fn u8(&self, name: &str) -> Result<Option<u8>, ApiError> {
        self.bounded(name, u8::MAX as u64)
            .map(|v| v.map(|v| v as u8))
    }

    fn bounded(&self, name: &str, max: u64) -> Result<Option<u64>, ApiError> {
        match self.u64(name)? {
            Some(value) if value > max => Err(ApiError::invalid_args(format!(
                "{} must be at most {}",
                name, max
            ))),
            other => Ok(other),
        }
    }

    pub fn str(&self, name: &str) -> Result<Option<&str>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.as_str())),
            Some(_) => Err(ApiError::invalid_args(format!("{} must be a string", name))),
        }
    }

    pub fn req_str(&self, name: &str) -> Result<&str, ApiError> {
        self.str(name)?
            .ok_or_else(|| ApiError::invalid_args(format!("Missing {}", name)))
    }

    pub fn bool(&self, name: &str) -> Result<Option<bool>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(ApiError::invalid_args(format!(
                "{} must be a boolean",
                name
            ))),
        }
    }

    pub fn bool_or(&self, name: &str, default: bool) -> Result<bool, ApiError> {
        Ok(self.bool(name)?.unwrap_or(default))
    }

    pub fn array(&self, name: &str) -> Result<Option<&Vec<Value>>, ApiError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::Array(items)) => Ok(Some(items)),
            Some(_) => Err(ApiError::invalid_args(format!("{} must be an array", name))),
        }
    }

    pub fn req_array(&self, name: &str) -> Result<&Vec<Value>, ApiError> {
        self.array(name)?
            .ok_or_else(|| ApiError::invalid_args(format!("Missing {}", name)))
    }

    /// A list of 64-bit integers, accepting the same number-or-string forms as
    /// [`Args::u64`].
    pub fn u64_array(&self, name: &str) -> Result<Option<Vec<u64>>, ApiError> {
        let Some(items) = self.array(name)? else {
            return Ok(None);
        };
        items
            .iter()
            .map(|item| match item {
                Value::Number(number) => number.as_u64().ok_or(()),
                Value::String(text) => text.parse::<u64>().map_err(|_| ()),
                _ => Err(()),
            })
            .collect::<Result<Vec<_>, ()>>()
            .map(Some)
            .map_err(|_| ApiError::invalid_args(format!("{} must be an array of integers", name)))
    }

    /// A value passed through verbatim (command payloads, attribute values).
    pub fn value(&self, name: &str) -> Option<Value> {
        self.0.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_args_object_reads_as_empty() {
        let request = Request::from_value(&json!({ "message_id": "1", "command": "server_info" }));
        assert!(request.args.is_empty());
        assert_eq!(request.command.as_deref(), Some("server_info"));
    }

    #[test]
    fn absent_message_id_becomes_empty_string() {
        let request = Request::from_value(&json!({ "command": "server_info" }));
        assert_eq!(request.message_id, json!(""));
    }

    #[test]
    fn explicit_null_is_treated_as_absent() {
        let args = Args::new(json!({ "node_id": null }));
        assert_eq!(args.u64("node_id").unwrap(), None);
    }

    #[test]
    fn node_ids_accept_numbers_and_decimal_strings() {
        let args = Args::new(json!({ "a": 18446744069414584320u64, "b": "112233" }));
        assert_eq!(args.u64("a").unwrap(), Some(18446744069414584320));
        assert_eq!(args.u64("b").unwrap(), Some(112233));
    }

    #[test]
    fn out_of_range_narrow_integers_are_rejected() {
        let args = Args::new(json!({ "endpoint_id": 70000 }));
        let error = args.u16("endpoint_id").unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
    }

    #[test]
    fn error_envelope_carries_code_and_details() {
        let envelope = response_envelope(&json!("7"), Err(ApiError::node_not_exists(4)));
        assert_eq!(envelope["message_id"], json!("7"));
        assert_eq!(envelope["error_code"], json!(5));
        assert_eq!(envelope["details"], json!("Node 4 does not exist"));
        assert!(envelope.get("result").is_none());
    }
}
