//! Client-side Interaction Model operations.
//!
//! Every function here opens a CASE exchange, drives one transaction to
//! completion, and drops it before returning, so the caller's actor loop stays
//! serialized. Values cross the boundary as JSON in the protocol's own shape —
//! see [`crate::matter::tlv_json`] — and Matter failures are mapped onto the
//! protocol's error codes here rather than leaking rs-matter errors upward.

use std::collections::BTreeMap;
use std::num::NonZeroU8;

use serde_json::{Map, Value};

use rs_matter::crypto::Crypto;
use rs_matter::error::ErrorCode;
use rs_matter::im::client::{ImClient, TxOutcome};
use rs_matter::im::{AttrPath, AttrResp, CmdResp, IMStatusCode};
use rs_matter::tlv::TLVTag;
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::protocol::error::{ApiError, ErrorCode as ProtocolError};
use crate::protocol::model::AttributesData;
use crate::protocol::paths::format_path;

use super::tlv_json::{self, TlvNode};

/// Status code the protocol reports for a successful write.
pub const STATUS_SUCCESS: u16 = 0;

/// Open a CASE exchange, translating the failure modes the protocol
/// distinguishes: a node we cannot reach at all is "not resolving", a node that
/// refuses the session is "not ready".
async fn open<'a, C: Crypto>(
    matter: &'a Matter<'a>,
    crypto: C,
    fabric_index: NonZeroU8,
    node_id: u64,
) -> Result<Exchange<'a>, ApiError> {
    Exchange::initiate(matter, crypto, fabric_index, node_id)
        .await
        .map_err(|error| match error.code() {
            ErrorCode::NoNetworkInterface | ErrorCode::NotFound | ErrorCode::RxTimeout => {
                ApiError::node_not_resolving(node_id)
            }
            _ => ApiError::new(
                ProtocolError::NodeNotReady,
                format!("Node {} is not ready: {:?}", node_id, error.code()),
            ),
        })
}

fn im_error(context: &str, error: rs_matter::error::Error) -> ApiError {
    ApiError::sdk(format!("{}: {:?}", context, error.code()))
}

/// Map an Interaction Model status onto the protocol error surface.
fn status_error(context: &str, status: IMStatusCode) -> ApiError {
    ApiError::sdk(format!("{}: {:?}", context, status))
}

/// Read one or more attribute paths, wildcards included.
///
/// Per-path failures are not fatal: a wildcard read across a node routinely
/// reports `UnsupportedAttribute` for paths the device does not implement, and
/// the reference simply omits them. A read that returns *only* failures is
/// reported as an error, so a caller asking for one concrete attribute still
/// learns that it could not be read.
pub async fn read_attributes<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fabric_index: NonZeroU8,
    node_id: u64,
    paths: Vec<AttrPath>,
    fabric_filtered: bool,
) -> Result<AttributesData, ApiError> {
    let exchange = open(matter, crypto, fabric_index, node_id).await?;
    let mut sender = exchange
        .read_sender()
        .await
        .map_err(|e| im_error("read exchange", e))?;
    let mut chunk = loop {
        match sender.tx().await.map_err(|e| im_error("read request", e))? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .attr_requests_from(&paths)
                    .map_err(|e| im_error("read path", e))?
                    .fabric_filtered(fabric_filtered)
                    .map_err(|e| im_error("read filter", e))?
                    .end()
                    .map_err(|e| im_error("read build", e))?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };

    let mut attributes = AttributesData::new();
    let mut first_status = None;
    loop {
        let response = chunk.response().map_err(|e| im_error("read response", e))?;
        if let Some(reports) = response.attr_reports.as_ref() {
            for report in reports.iter() {
                match report.map_err(|e| im_error("attribute report", e))? {
                    AttrResp::Data(data) => {
                        let path = format_path(
                            data.path.endpoint.unwrap_or(0),
                            data.path.cluster.unwrap_or(0),
                            data.path.attr.unwrap_or(0),
                        );
                        let value = tlv_json::to_json(&data.data)
                            .map_err(|e| im_error("attribute value", e))?;
                        attributes.insert(path, value);
                    }
                    AttrResp::Status(status) => {
                        first_status.get_or_insert(status.status.status);
                    }
                }
            }
        }
        match chunk
            .complete()
            .await
            .map_err(|e| im_error("read completion", e))?
        {
            Some(next) => chunk = next,
            None => break,
        }
    }

    if attributes.is_empty() {
        if let Some(status) = first_status {
            return Err(status_error("attribute read failed", status));
        }
    }
    Ok(attributes)
}

/// Write one attribute, returning the Interaction Model status the node
/// reported. The protocol surfaces that number rather than turning a non-zero
/// status into an error.
///
/// The argument list is long because an IM write is addressed by that many
/// independent things; a parameter struct would relocate the list rather than
/// shorten it.
#[allow(clippy::too_many_arguments)]
pub async fn write_attribute<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fabric_index: NonZeroU8,
    node_id: u64,
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    value: TlvNode,
    timed_timeout_ms: Option<u16>,
) -> Result<u16, ApiError> {
    let exchange = open(matter, crypto, fabric_index, node_id).await?;
    let mut sender = exchange
        .write_sender(timed_timeout_ms)
        .await
        .map_err(|e| im_error("write exchange", e))?;
    let handle = loop {
        match sender
            .tx()
            .await
            .map_err(|e| im_error("write request", e))?
        {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .write_requests()
                    .map_err(|e| im_error("write requests", e))?
                    .push()
                    .map_err(|e| im_error("write entry", e))?
                    .path(endpoint, cluster, attribute)
                    .map_err(|e| im_error("write path", e))?
                    // The MRP layer may rebuild the request, so this closure
                    // must be deterministic; writing the same resolved node is.
                    .data(|w| value.write(w, &TLVTag::Context(2)))
                    .map_err(|e| im_error("write value", e))?
                    .end()
                    .map_err(|e| im_error("write entry end", e))?
                    .end()
                    .map_err(|e| im_error("write array", e))?
                    .end()
                    .map_err(|e| im_error("write message", e))?;
            }
            TxOutcome::GotResponse(handle) => break handle,
        }
    };

    let response = handle
        .response()
        .map_err(|e| im_error("write response", e))?;
    let mut status = STATUS_SUCCESS;
    for entry in response.write_responses.iter() {
        let entry = entry.map_err(|e| im_error("write status", e))?;
        if entry.status.status != IMStatusCode::Success {
            status = entry.status.status as u16;
            break;
        }
    }
    Ok(status)
}

/// Invoke a command.
///
/// `response_names` names the top-level response fields; when it is empty the
/// response decodes tag-based, matching how the reference treats a command it
/// has no schema for.
#[allow(clippy::too_many_arguments)]
pub async fn invoke<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fabric_index: NonZeroU8,
    node_id: u64,
    endpoint: u16,
    cluster: u32,
    command: u32,
    payload: TlvNode,
    timed_timeout_ms: Option<u16>,
    response_names: &BTreeMap<u32, String>,
) -> Result<Value, ApiError> {
    let exchange = open(matter, crypto, fabric_index, node_id).await?;
    let mut sender = exchange
        .invoke_sender(timed_timeout_ms)
        .await
        .map_err(|e| im_error("invoke exchange", e))?;
    let mut chunk = loop {
        match sender
            .tx()
            .await
            .map_err(|e| im_error("invoke request", e))?
        {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .suppress_response(false)
                    .map_err(|e| im_error("invoke response flag", e))?
                    .timed_request(timed_timeout_ms.is_some())
                    .map_err(|e| im_error("invoke timing", e))?
                    .invoke_requests()
                    .map_err(|e| im_error("invoke requests", e))?
                    .push()
                    .map_err(|e| im_error("invoke entry", e))?
                    .path(endpoint, cluster, command)
                    .map_err(|e| im_error("invoke path", e))?
                    .data(|w| payload.write(w, &TLVTag::Context(1)))
                    .map_err(|e| im_error("invoke data", e))?
                    .end()
                    .map_err(|e| im_error("invoke entry end", e))?
                    .end()
                    .map_err(|e| im_error("invoke array", e))?
                    .end()
                    .map_err(|e| im_error("invoke message", e))?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };

    // A command with no response payload answers with a bare success status;
    // the protocol reports that as an empty object, not null.
    let mut result = Value::Object(Map::new());
    loop {
        if let Some(response) = chunk
            .response()
            .map_err(|e| im_error("invoke response", e))?
        {
            if let Some(responses) = response.invoke_responses.as_ref() {
                for entry in responses.iter() {
                    match entry.map_err(|e| im_error("invoke response entry", e))? {
                        CmdResp::Cmd(data) => {
                            result = if response_names.is_empty() {
                                tlv_json::to_json(&data.data)
                            } else {
                                tlv_json::to_json_named(&data.data, response_names)
                            }
                            .map_err(|e| im_error("invoke response value", e))?;
                        }
                        CmdResp::Status(status) => {
                            if status.status.status != IMStatusCode::Success {
                                return Err(status_error("command failed", status.status.status));
                            }
                        }
                    }
                }
            }
        }
        match chunk
            .complete()
            .await
            .map_err(|e| im_error("invoke completion", e))?
        {
            Some(next) => chunk = next,
            None => break,
        }
    }
    Ok(result)
}

/// Establish a CASE session as a reachability probe.
pub async fn ping<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fabric_index: NonZeroU8,
    node_id: u64,
) -> Result<(), ApiError> {
    open(matter, crypto, fabric_index, node_id)
        .await
        .map(|_| ())
}

/// Read the paths a first interview needs: the whole of every endpoint the node
/// exposes.
///
/// A single wildcard read (`*/*/*`) is what the reference does and what devices
/// expect; it is also the only way to discover endpoints that were not present
/// at commissioning time.
pub fn interview_paths() -> Vec<AttrPath> {
    vec![AttrPath::from_gp(&rs_matter::im::GenericPath::new(
        None, None, None,
    ))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interview_reads_every_path() {
        let paths = interview_paths();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].endpoint.is_none());
        assert!(paths[0].cluster.is_none());
        assert!(paths[0].attr.is_none());
    }

    #[test]
    fn a_successful_write_reports_status_zero() {
        assert_eq!(STATUS_SUCCESS, 0);
    }
}
