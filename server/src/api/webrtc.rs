//! WebRTC provider commands.
//!
//! Routing `ProvideOffer` / `SolicitOffer` is only half of the exchange: the
//! camera returns its answer and ICE candidates by invoking
//! `WebRTCTransportRequestor` on the controller, which this node does not host
//! — it accepts no incoming exchange at all — so there is nothing to feed the
//! `webrtc_callback` event stream or the session bookkeeping behind it. The
//! command reports the SDK error with a reason instead of appearing to start a
//! session that can never be completed.

use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::message::Args;

use super::{require_node, CallContext};

pub async fn send_provider_command(args: &Args, context: CallContext<'_>) -> ApiResult {
    // Validate the request anyway: a caller with a bad node id should learn
    // that first, and the error it gets should not depend on what is
    // implemented behind it.
    let node_id = require_node(args, context)?;
    let _endpoint = args.req_u16("endpoint_id")?;
    let command = args.req_str("command_name")?;
    if !matches!(command, "ProvideOffer" | "SolicitOffer") {
        return Err(ApiError::invalid_args(format!(
            "Unknown WebRTC provider command '{}'",
            command
        )));
    }

    Err(ApiError::sdk(format!(
        "WebRTC is not supported by this server, so '{}' cannot be sent to node {}",
        command, node_id
    )))
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
    fn arguments_are_validated_before_the_capability_is_reported() {
        let context = context_with_node();
        let args =
            Args::new(json!({ "node_id": 99, "endpoint_id": 1, "command_name": "ProvideOffer" }));
        assert_eq!(
            block_on(send_provider_command(&args, call(&context)))
                .unwrap_err()
                .code
                .as_i64(),
            5
        );

        let args = Args::new(json!({ "node_id": 1, "endpoint_id": 1, "command_name": "Nonsense" }));
        assert_eq!(
            block_on(send_provider_command(&args, call(&context)))
                .unwrap_err()
                .code
                .as_i64(),
            8
        );
    }

    #[test]
    fn a_valid_request_reports_the_missing_capability() {
        let context = context_with_node();
        let args = Args::new(
            json!({ "node_id": 1, "endpoint_id": 1, "command_name": "ProvideOffer", "payload": {} }),
        );
        let error = block_on(send_provider_command(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 7);
        assert!(error.details.contains("WebRTC is not supported"));
    }
}
