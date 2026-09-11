//! WebRTC provider commands.
//!
//! One half of a camera negotiation: the client's request goes out to the
//! camera's WebRTC Transport Provider cluster from here. The camera's reply
//! comes back the other way, as an invoke on the WebRTC Transport Requestor
//! cluster `matter::webrtc` hosts, and reaches the client as a
//! `webrtc_callback` event.
//!
//! Only `SolicitOffer` and `ProvideOffer` are accepted, as the reference
//! accepts. The rest of the cluster — answering, trickling candidates, ending
//! a session — is `device_command`'s job: those are ordinary invokes with
//! nothing session-shaped about them, and narrowing this command to the two
//! that start a negotiation is what the reference's own signature says.

use crate::matter::{clusters, tlv_json};
use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::message::Args;

use super::{require_node, CallContext};

/// The camera's WebRTC Transport Provider cluster.
const WEBRTC_PROVIDER_CLUSTER: u32 = 0x0553;

/// The commands a client may send through this route.
const NEGOTIATION_COMMANDS: [&str; 2] = ["ProvideOffer", "SolicitOffer"];

pub async fn send_provider_command(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let endpoint = args.req_u16("endpoint_id")?;
    let command_name = args.req_str("command_name")?;
    let payload = args.value("payload").unwrap_or_default();

    if !NEGOTIATION_COMMANDS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(command_name))
    {
        return Err(ApiError::invalid_args(format!(
            "Unknown WebRTC provider command '{}'; expected one of {}",
            command_name,
            NEGOTIATION_COMMANDS.join(", ")
        )));
    }

    // The cluster metadata resolves the command and its payload field names,
    // exactly as `device_command` does — an SDP is a string field like any
    // other, and the session ids are integers.
    let cluster = clusters::cluster(WEBRTC_PROVIDER_CLUSTER).ok_or_else(|| {
        ApiError::sdk("This build has no metadata for the WebRTC Transport Provider cluster")
    })?;
    let command = cluster.command(command_name).ok_or_else(|| {
        ApiError::invalid_args(format!(
            "The WebRTC Transport Provider cluster has no '{}' command",
            command_name
        ))
    })?;
    let encoded = tlv_json::command_payload_from_json(command, &payload)?;

    context
        .server
        .matter
        .invoke(
            node_id,
            endpoint,
            WEBRTC_PROVIDER_CLUSTER,
            command.id,
            encoded,
            None,
            command.response_fields.clone(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::{call, test_context};
    use crate::protocol::model::MatterNodeData;
    use crate::storage::StoredNode;
    use futures_lite::future::block_on;
    use serde_json::json;

    fn context_with_node() -> crate::api::ServerContext {
        let context = test_context();
        context.nodes.upsert(StoredNode::new(MatterNodeData::new(
            1,
            "2026-01-01T00:00:00.000Z".into(),
        )));
        context
    }

    #[test]
    fn only_the_two_negotiation_commands_are_accepted() {
        let context = context_with_node();
        let args = Args::new(json!({
            "node_id": 1,
            "endpoint_id": 1,
            "command_name": "ProvideAnswer",
            "payload": {},
        }));
        let error = block_on(send_provider_command(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(error.details.contains("Unknown WebRTC provider command"));
    }

    /// The payload is resolved before anything is sent, so a field the cluster
    /// does not have is an argument error rather than a failed invoke.
    #[test]
    fn an_unknown_payload_field_is_rejected_by_name() {
        let context = context_with_node();
        let args = Args::new(json!({
            "node_id": 1,
            "endpoint_id": 1,
            "command_name": "ProvideOffer",
            "payload": { "sessionDescription": "v=0" },
        }));
        let error = block_on(send_provider_command(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(
            error.details.contains("sessionDescription"),
            "{}",
            error.details
        );
    }

    /// A well-formed request gets as far as the radio, which this test does
    /// not have.
    #[test]
    fn a_well_formed_offer_reaches_the_device() {
        let context = context_with_node();
        let args = Args::new(json!({
            "node_id": 1,
            "endpoint_id": 1,
            "command_name": "SolicitOffer",
            "payload": { "streamUsage": 1, "originatingEndpointID": 1 },
        }));
        let error = block_on(send_provider_command(&args, call(&context))).unwrap_err();
        // The SDK error from the absent actor, not an argument error.
        assert_eq!(error.code.as_i64(), 7, "{}", error.details);
    }

    #[test]
    fn the_command_and_its_endpoint_are_required() {
        let context = context_with_node();
        let args = Args::new(json!({ "node_id": 1, "command_name": "ProvideOffer" }));
        assert_eq!(
            block_on(send_provider_command(&args, call(&context)))
                .unwrap_err()
                .code
                .as_i64(),
            8
        );
    }
}
