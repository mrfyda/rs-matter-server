//! One client connection: the WebSocket lifecycle and the OTA upload endpoint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_tungstenite::tungstenite::Message;
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures_lite::StreamExt;
use futures_util::SinkExt;
use serde_json::{json, Value};

use crate::api::{self, CallContext, ServerContext};
use crate::protocol::events::{Event, EventSubscriptions};
use crate::protocol::message::{response_envelope, Request};

use super::http::{self, Prefixed, RequestHead};

/// Connection ids only have to be unique within a run; they scope fabric-label
/// ownership and appear in logs.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Serve one accepted connection, whichever endpoint it turns out to be for.
pub async fn serve<S>(stream: S, peer: String, context: Arc<ServerContext>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = stream;
    let (head, raw) = match http::read_head(&mut stream).await {
        Ok(parsed) => parsed,
        Err(error) => {
            log::debug!("{}: could not read the request head: {}", peer, error);
            return;
        }
    };

    if head.is_websocket_upgrade() {
        serve_websocket(Prefixed::new(raw, stream), peer, context).await;
        return;
    }

    serve_http(stream, head, peer, context).await;
}

/// The WebSocket lifecycle: greet, then interleave client requests with the
/// event stream until either side goes away.
async fn serve_websocket<S>(stream: S, peer: String, context: Arc<ServerContext>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut websocket = match async_tungstenite::accept_async(stream).await {
        Ok(websocket) => websocket,
        Err(error) => {
            log::warn!("{}: WebSocket handshake failed: {}", peer, error);
            return;
        }
    };

    let connection_id = next_connection_id();
    let mut subscriptions = EventSubscriptions::default();
    // Subscribe before the greeting so no event is missed between the two.
    let events = context.events.subscribe();

    log::info!("{}: connection {} established", peer, connection_id);

    // The protocol opens with an unsolicited `server_info`; clients wait for
    // it before sending anything.
    match api::server_info::build_server_info(&context).await {
        Ok(info) => {
            let greeting = serde_json::to_string(&info).unwrap_or_default();
            if websocket.send(Message::Text(greeting)).await.is_err() {
                return;
            }
        }
        Err(error) => {
            log::error!("{}: could not build server_info: {}", peer, error);
            return;
        }
    }

    loop {
        let incoming = async { Incoming::Client(websocket.next().await) };
        let outgoing = async { Incoming::Event(events.recv().await.ok()) };

        match futures_lite::future::or(incoming, outgoing).await {
            Incoming::Client(Some(Ok(message))) => {
                if message.is_close() {
                    break;
                }
                let Ok(text) = message.into_text() else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                let response =
                    handle_request(&text, &context, connection_id, &peer, &mut subscriptions).await;
                let encoded = serde_json::to_string(&response).unwrap_or_default();
                if websocket.send(Message::Text(encoded)).await.is_err() {
                    break;
                }
            }
            Incoming::Client(Some(Err(error))) => {
                log::debug!("{}: connection closed: {}", peer, error);
                break;
            }
            Incoming::Client(None) => break,
            Incoming::Event(Some(event)) => {
                if !subscriptions.accepts(&event) {
                    continue;
                }
                let encoded = serde_json::to_string(&event.payload).unwrap_or_default();
                if websocket.send(Message::Text(encoded)).await.is_err() {
                    break;
                }
            }
            // The event bus dropped this connection, which happens when it
            // could not keep up. Its view is stale, so end the connection and
            // let the client reconnect and re-sync.
            Incoming::Event(None) => {
                log::warn!(
                    "{}: dropped from the event stream; closing so the client can re-sync",
                    peer
                );
                break;
            }
        }
    }

    context.release_fabric_label(connection_id);
    log::info!("{}: connection {} closed", peer, connection_id);
}

enum Incoming {
    Client(Option<Result<Message, async_tungstenite::tungstenite::Error>>),
    Event(Option<Event>),
}

/// Parse, dispatch, and envelope one request.
async fn handle_request(
    text: &str,
    context: &Arc<ServerContext>,
    connection_id: u64,
    peer: &str,
    subscriptions: &mut EventSubscriptions,
) -> Value {
    let Ok(raw) = serde_json::from_str::<Value>(text) else {
        // There is no message id to correlate a malformed frame with, so the
        // error is reported against the empty id the protocol uses.
        return json!({
            "message_id": "",
            "error_code": crate::protocol::ErrorCode::InvalidArguments.as_i64(),
            "details": "Invalid JSON",
        });
    };

    let request = Request::from_value(&raw);
    let Some(command) = request.command.clone() else {
        return response_envelope(
            &request.message_id,
            Err(crate::protocol::ApiError::new(
                crate::protocol::ErrorCode::InvalidCommand,
                "Missing command",
            )),
        );
    };

    // Opt-ins latch on the attempt, not on success: issuing the command is
    // what proves the client understands the event family.
    subscriptions.observe_command(&command);

    log::debug!("{}: request {} ({})", peer, command, request.message_id);

    let call = CallContext {
        server: context,
        connection_id,
        peer,
    };
    let result = api::dispatch(&command, &request.args, call).await;
    if let Err(error) = &result {
        log::info!("{}: {} failed: {}", peer, command, error);
    }
    response_envelope(&request.message_id, result)
}

/// The OTA upload endpoint, plus the errors for everything else.
async fn serve_http<S>(mut stream: S, head: RequestHead, peer: String, context: Arc<ServerContext>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if head.path == "/health" || head.path.starts_with("/health?") {
        let response = if head.method.eq_ignore_ascii_case("GET") {
            // Reporting the node count makes the check useful beyond
            // liveness: an operator can see the controller came back with its
            // fabric intact rather than empty.
            http::json_response(
                200,
                "OK",
                &json!({
                    "status": "ok",
                    "schema_version": crate::protocol::SCHEMA_VERSION,
                    "nodes": context.nodes.len(),
                })
                .to_string(),
            )
        } else {
            http::method_not_allowed("GET")
        };
        let _ = stream.write_all(&response).await;
        let _ = stream.flush().await;
        return;
    }

    let response = match upload_id(&head.path) {
        Some(upload_id) => {
            if !head.method.eq_ignore_ascii_case("POST") {
                http::method_not_allowed("POST")
            } else if !context.runtime.ota_enabled {
                http::json_response(
                    400,
                    "Bad Request",
                    &json!({
                        "error_code": crate::protocol::ErrorCode::OtaUploadError.as_i64(),
                        "message": "OTA support is disabled on this server",
                    })
                    .to_string(),
                )
            } else {
                handle_ota_upload(&mut stream, &head, upload_id, &peer, &context).await
            }
        }
        None => http::json_response(
            404,
            "Not Found",
            &json!({ "error": "Not found" }).to_string(),
        ),
    };

    let _ = stream.write_all(&response).await;
    let _ = stream.flush().await;
}

/// `/ota-upload/<id>` where the id is the 32 hex characters `initiate_ota_upload`
/// handed out.
fn upload_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/ota-upload/")?;
    let id = id.split(['?', '#']).next().unwrap_or(id);
    if id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(id)
    } else {
        None
    }
}

async fn handle_ota_upload<S>(
    stream: &mut S,
    head: &RequestHead,
    upload_id: &str,
    peer: &str,
    context: &Arc<ServerContext>,
) -> Vec<u8>
where
    S: AsyncRead + Unpin,
{
    // Refuse an oversized upload before reading it: the point of the limit is
    // not to buffer the body at all.
    if let Some(length) = head.content_length() {
        if length > api::ota::MAX_UPLOAD_SIZE {
            return http::json_response(
                413,
                "Payload Too Large",
                &json!({ "error": "The image exceeds the server's upload size limit" }).to_string(),
            );
        }
    }

    if let Err(error) = context.ota.redeem(upload_id, peer) {
        return http::json_response(
            400,
            "Bad Request",
            &json!({ "error_code": error.code.as_i64(), "message": error.details }).to_string(),
        );
    }

    let mut body = head.body_prefix.clone();
    let expected = head.content_length().unwrap_or(0) as usize;
    let mut chunk = vec![0u8; 64 * 1024];
    while body.len() < expected {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                body.extend_from_slice(&chunk[..read]);
                if body.len() as u64 > api::ota::MAX_UPLOAD_SIZE {
                    return http::json_response(
                        413,
                        "Payload Too Large",
                        &json!({ "error": "The image exceeds the server's upload size limit" })
                            .to_string(),
                    );
                }
            }
            Err(error) => {
                return http::json_response(
                    400,
                    "Bad Request",
                    &json!({
                        "error_code": crate::protocol::ErrorCode::OtaUploadError.as_i64(),
                        "message": format!("The upload could not be read: {}", error),
                    })
                    .to_string(),
                )
            }
        }
    }

    match api::ota::parse_ota_header(&body) {
        Ok(version) => {
            log::info!(
                "Stored OTA image for vendor 0x{:04x} product 0x{:04x} version {}",
                version.vid,
                version.pid,
                version.software_version
            );
            context.ota.store(api::ota::StoredImage {
                version: version.clone(),
                bytes: body,
            });
            http::json_response(
                200,
                "OK",
                &serde_json::to_string(&version).unwrap_or_default(),
            )
        }
        Err(error) => http::json_response(
            400,
            "Bad Request",
            &json!({ "error_code": error.code.as_i64(), "message": error.details }).to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_ids_must_look_like_the_ones_we_issue() {
        assert_eq!(
            upload_id("/ota-upload/3f9a1c2e8b7d4a10f6c9e0b2d4a17853"),
            Some("3f9a1c2e8b7d4a10f6c9e0b2d4a17853")
        );
        assert_eq!(upload_id("/ota-upload/short"), None);
        assert_eq!(upload_id("/ota-upload/"), None);
        assert_eq!(upload_id("/ws"), None);
        // A query string is not part of the id.
        assert_eq!(
            upload_id("/ota-upload/3f9a1c2e8b7d4a10f6c9e0b2d4a17853?x=1"),
            Some("3f9a1c2e8b7d4a10f6c9e0b2d4a17853")
        );
    }

    #[test]
    fn the_health_path_is_recognised_with_and_without_a_query() {
        // The health endpoint is matched before the OTA route, so it must not
        // be mistaken for an upload id.
        assert_eq!(upload_id("/health"), None);
        assert_eq!(upload_id("/health?probe=1"), None);
    }

    #[test]
    fn connection_ids_are_unique() {
        let first = next_connection_id();
        let second = next_connection_id();
        assert_ne!(first, second);
    }

    #[test]
    fn a_malformed_frame_is_answered_with_an_argument_error() {
        let context = Arc::new(crate::api::tests_support::test_context());
        let mut subscriptions = EventSubscriptions::default();
        let response = futures_lite::future::block_on(handle_request(
            "not json",
            &context,
            1,
            "peer",
            &mut subscriptions,
        ));
        assert_eq!(response["error_code"], json!(8));
        assert_eq!(response["message_id"], json!(""));
    }

    #[test]
    fn a_frame_without_a_command_is_an_invalid_command() {
        let context = Arc::new(crate::api::tests_support::test_context());
        let mut subscriptions = EventSubscriptions::default();
        let response = futures_lite::future::block_on(handle_request(
            "{\"message_id\":\"7\"}",
            &context,
            1,
            "peer",
            &mut subscriptions,
        ));
        assert_eq!(response["message_id"], json!("7"));
        assert_eq!(response["error_code"], json!(9));
    }

    #[test]
    fn an_unknown_command_reports_its_name() {
        let context = Arc::new(crate::api::tests_support::test_context());
        let mut subscriptions = EventSubscriptions::default();
        let response = futures_lite::future::block_on(handle_request(
            "{\"message_id\":\"1\",\"command\":\"fly\"}",
            &context,
            1,
            "peer",
            &mut subscriptions,
        ));
        assert_eq!(response["error_code"], json!(9));
        assert_eq!(response["details"], json!("Unknown command: fly"));
    }

    #[test]
    fn start_listening_latches_the_event_subscription() {
        let context = Arc::new(crate::api::tests_support::test_context());
        let mut subscriptions = EventSubscriptions::default();
        assert!(!subscriptions.listening);
        futures_lite::future::block_on(handle_request(
            "{\"message_id\":\"1\",\"command\":\"start_listening\"}",
            &context,
            1,
            "peer",
            &mut subscriptions,
        ));
        assert!(subscriptions.listening);
        assert!(!subscriptions.network_topology);
    }

    #[test]
    fn an_opt_in_command_latches_even_when_it_errors() {
        let context = Arc::new(crate::api::tests_support::test_context());
        let mut subscriptions = EventSubscriptions::default();
        futures_lite::future::block_on(handle_request(
            "{\"message_id\":\"1\",\"command\":\"get_thread_diagnostics\",\"args\":{\"ext_pan_id\":\"bad\"}}",
            &context,
            1,
            "peer",
            &mut subscriptions,
        ));
        assert!(subscriptions.thread_diagnostics);
    }
}
