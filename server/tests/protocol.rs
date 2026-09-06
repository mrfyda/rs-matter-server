//! Contract tests for the matterjs-server WebSocket protocol.
//!
//! These assert the exact wire shapes a client sees — field names, response
//! envelopes, error codes, and who receives which events — rather than only
//! that a request succeeded. They run against a server with no Matter actor
//! behind it, so anything needing a radio reports the SDK error while the whole
//! protocol surface stays exercisable.

use std::time::Duration;

use futures_util::sink::SinkExt;
use futures_util::StreamExt;
use rs_matter_server::api::COMMANDS;
use rs_matter_server::protocol::model::MatterNodeData;
use rs_matter_server::storage::StoredNode;
use rs_matter_server::ws::{test_start, test_start_with, TestServer};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{connect_async, tungstenite as ws_lib, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect and consume the unsolicited `server_info` greeting.
async fn connect(port: u16) -> (Socket, Value) {
    let (mut socket, _) = connect_async(format!("ws://127.0.0.1:{}/ws", port))
        .await
        .expect("connect");
    let greeting = next_json(&mut socket).await;
    (socket, greeting)
}

async fn next_json(socket: &mut Socket) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("a message within five seconds")
        .expect("a message")
        .expect("a readable message");
    match message {
        ws_lib::Message::Text(text) => serde_json::from_str(&text).expect("valid JSON"),
        other => panic!("unexpected frame: {:?}", other),
    }
}

/// Receive the next frame, or `None` if nothing arrives promptly.
async fn try_next_json(socket: &mut Socket) -> Option<Value> {
    match tokio::time::timeout(Duration::from_millis(300), socket.next()).await {
        Ok(Some(Ok(ws_lib::Message::Text(text)))) => Some(serde_json::from_str(&text).unwrap()),
        _ => None,
    }
}

async fn request(socket: &mut Socket, message_id: &str, command: &str, args: Value) -> Value {
    let frame = json!({ "message_id": message_id, "command": command, "args": args });
    socket
        .send(ws_lib::Message::Text(frame.to_string()))
        .await
        .expect("send");
    next_json(socket).await
}

fn seeded_node(node_id: u64) -> StoredNode {
    let mut node = MatterNodeData::new(node_id, "2026-01-01T00:00:00.000Z".into());
    node.attributes.insert("0/40/1".into(), json!("ACME"));
    node.attributes.insert("0/40/3".into(), json!("Test Plug"));
    node.attributes.insert("1/6/0".into(), json!(true));
    let mut stored = StoredNode::new(node);
    stored.ip_addresses = vec!["fd00::1".into()];
    stored
}

async fn server_with_node() -> TestServer {
    test_start_with(|context| {
        context.nodes.upsert(seeded_node(1));
        context
    })
    .await
}

#[tokio::test]
async fn the_greeting_carries_the_documented_server_info() {
    let server = test_start().await;
    let (_socket, info) = connect(server.port()).await;

    assert!(info["fabric_id"].is_number());
    assert!(info["compressed_fabric_id"].is_number());
    assert_eq!(info["schema_version"], json!(13));
    assert_eq!(info["min_supported_schema_version"], json!(11));
    assert!(info["sdk_version"].as_str().unwrap().contains("rs-matter"));
    assert_eq!(info["wifi_credentials_set"], json!(false));
    assert_eq!(info["thread_credentials_set"], json!(false));
    assert_eq!(info["bluetooth_enabled"], json!(false));
    // OHF extensions.
    assert_eq!(info["fabric_index"], json!(1));
    assert_eq!(info["controller_node_id"], json!(112233));
    // The greeting is a bare object, not a response envelope.
    assert!(info.get("message_id").is_none());
    assert!(info.get("result").is_none());
}

#[tokio::test]
async fn responses_echo_the_message_id_they_were_asked_with() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;

    let response = request(&mut socket, "abc-123", "server_info", json!({})).await;
    assert_eq!(response["message_id"], json!("abc-123"));
    assert!(response.get("result").is_some());
    assert!(response.get("error_code").is_none());
}

#[tokio::test]
async fn an_unknown_command_is_reported_as_invalid_command() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;

    let response = request(&mut socket, "1", "levitate", json!({})).await;
    assert_eq!(response["error_code"], json!(9));
    assert_eq!(response["details"], json!("Unknown command: levitate"));
    assert!(response.get("result").is_none());
}

/// Every command the server advertises must be routed. A name that fell
/// through to the catch-all would answer "unknown command" while still being
/// listed as supported, which is exactly the failure this guards against.
#[tokio::test]
async fn every_advertised_command_is_routed() {
    let server = server_with_node().await;
    let (mut socket, _) = connect(server.port()).await;

    for (index, command) in COMMANDS.iter().enumerate() {
        let response = request(
            &mut socket,
            &index.to_string(),
            command,
            json!({ "node_id": 1 }),
        )
        .await;
        assert_ne!(
            response["error_code"],
            json!(9),
            "command '{}' is advertised but not routed: {}",
            command,
            response
        );
    }
}

#[tokio::test]
async fn diagnostics_uses_the_documented_envelope() {
    let server = server_with_node().await;
    let (mut socket, _) = connect(server.port()).await;

    let response = request(&mut socket, "1", "diagnostics", json!({})).await;
    let result = &response["result"];
    assert!(result["info"]["schema_version"].is_number());
    assert_eq!(result["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(result["events"], json!([]));
    // The Python-era key names must not reappear.
    assert!(result.get("server_info").is_none());
    assert!(result.get("recent_events").is_none());
}

#[tokio::test]
async fn nodes_are_reported_in_the_wire_shape() {
    let server = server_with_node().await;
    let (mut socket, _) = connect(server.port()).await;

    let response = request(&mut socket, "1", "get_node", json!({ "node_id": 1 })).await;
    let node = &response["result"];
    assert_eq!(node["node_id"], json!(1));
    assert_eq!(node["available"], json!(true));
    assert_eq!(node["is_bridge"], json!(false));
    assert_eq!(node["attribute_subscriptions"], json!([]));
    assert_eq!(node["attributes"]["0/40/1"], json!("ACME"));
    assert!(node["date_commissioned"].is_string());
    assert!(node["last_interview"].is_string());
    // Controller-internal state must not leak onto the wire.
    assert!(node.get("ip_addresses").is_none());
    assert!(node.get("device_fabric_index").is_none());
}

#[tokio::test]
async fn a_missing_node_reports_node_not_exists() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;

    let response = request(&mut socket, "1", "get_node", json!({ "node_id": 42 })).await;
    assert_eq!(response["error_code"], json!(5));
    assert_eq!(response["details"], json!("Node 42 does not exist"));
}

#[tokio::test]
async fn events_reach_only_connections_that_started_listening() {
    let server = server_with_node().await;
    let (mut listener, _) = connect(server.port()).await;
    let (mut bystander, _) = connect(server.port()).await;

    let response = request(&mut listener, "1", "start_listening", json!({})).await;
    assert_eq!(response["result"].as_array().unwrap().len(), 1);

    server
        .context
        .events
        .publish(rs_matter_server::protocol::Event::node_removed(1));

    let event = next_json(&mut listener).await;
    assert_eq!(event["event"], json!("node_removed"));
    assert_eq!(event["data"], json!(1));
    assert!(
        try_next_json(&mut bystander).await.is_none(),
        "a connection that never listened must receive nothing"
    );
}

#[tokio::test]
async fn topology_events_need_their_opt_in_command() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;
    request(&mut socket, "1", "start_listening", json!({})).await;

    let topology = rs_matter_server::protocol::model::NetworkTopology::default();
    server
        .context
        .events
        .publish(rs_matter_server::protocol::Event::network_topology_updated(
            &topology,
        ));
    assert!(
        try_next_json(&mut socket).await.is_none(),
        "a pre-schema-13 client must not receive topology events"
    );

    // Issuing the command opts this connection in.
    request(&mut socket, "2", "get_network_topology", json!({})).await;
    server
        .context
        .events
        .publish(rs_matter_server::protocol::Event::network_topology_updated(
            &topology,
        ));
    let event = next_json(&mut socket).await;
    assert_eq!(event["event"], json!("network_topology_updated"));
}

#[tokio::test]
async fn credentials_are_stored_write_only_and_announced() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;
    request(&mut socket, "1", "start_listening", json!({})).await;

    let response = request(
        &mut socket,
        "2",
        "set_wifi_credentials",
        json!({ "ssid": "home", "credentials": "hunter2" }),
    )
    .await;
    assert_eq!(response["result"], json!({}));

    // The change is announced to listeners.
    let event = next_json(&mut socket).await;
    assert_eq!(event["event"], json!("server_info_updated"));
    assert_eq!(event["data"]["wifi_credentials_set"], json!(true));
    assert_eq!(event["data"]["wifi_ssid"], json!("home"));

    let listed = request(&mut socket, "3", "get_all_credentials", json!({})).await;
    let rendered = listed.to_string();
    assert!(rendered.contains("home"));
    assert!(
        !rendered.contains("hunter2"),
        "secrets must never be listed"
    );
    assert_eq!(listed["result"]["thread"][0]["id"], json!("default"));
}

#[tokio::test]
async fn the_fabric_label_round_trips_and_answers_with_null() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;

    let response = request(&mut socket, "1", "get_fabric_label", json!({})).await;
    assert_eq!(
        response["result"],
        json!({ "fabric_label": "HomeAssistant" })
    );

    // Setting it needs the Matter actor, which the test server does not run;
    // the point here is the response *shape* of the read path.
    let response = request(&mut socket, "2", "get_fabric_label", json!({})).await;
    assert!(response["result"]["fabric_label"].is_string());
}

#[tokio::test]
async fn importing_a_test_node_announces_it() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;
    request(&mut socket, "1", "start_listening", json!({})).await;

    let dump = json!({
        "data": {
            "node": {
                "node_id": 4,
                "date_commissioned": "2026-01-01T00:00:00.000Z",
                "last_interview": "2026-01-02T00:00:00.000Z",
                "interview_version": 6,
                "available": true,
                "is_bridge": false,
                "attributes": { "0/40/1": "ACME" }
            }
        }
    });
    let response = request(
        &mut socket,
        "2",
        "import_test_node",
        json!({ "dump": dump.to_string() }),
    )
    .await;
    assert_eq!(response["result"], Value::Null);

    let event = next_json(&mut socket).await;
    assert_eq!(event["event"], json!("node_added"));
    assert_eq!(event["data"]["node_id"], json!(0xFFFF_FFFE_0000_0000u64));
    assert_eq!(event["data"]["attributes"]["0/40/1"], json!("ACME"));
}

#[tokio::test]
async fn argument_errors_are_reported_before_any_matter_work() {
    let server = server_with_node().await;
    let (mut socket, _) = connect(server.port()).await;

    let cases: Vec<(&str, Value, i64)> = vec![
        ("read_attribute", json!({ "node_id": 1 }), 8),
        (
            "write_attribute",
            json!({ "node_id": 1, "attribute_path": "1/6/*", "value": 1 }),
            8,
        ),
        (
            "device_command",
            json!({ "node_id": 1, "endpoint_id": 1, "cluster_id": 6, "command_name": "explode" }),
            8,
        ),
        ("get_node_ip_addresses", json!({}), 8),
        ("commission_with_code", json!({ "code": "" }), 8),
        ("set_loglevel", json!({ "console_loglevel": "loud" }), 8),
        ("set_thread_dataset", json!({ "dataset": "zz" }), 8),
        (
            "remove_matter_fabric",
            json!({ "node_id": 1, "fabric_index": 0 }),
            8,
        ),
    ];

    for (index, (command, args, expected)) in cases.into_iter().enumerate() {
        let response = request(&mut socket, &index.to_string(), command, args).await;
        assert_eq!(
            response["error_code"],
            json!(expected),
            "unexpected code for {}: {}",
            command,
            response
        );
        assert!(response["details"].is_string());
    }
}

#[tokio::test]
async fn a_malformed_frame_does_not_close_the_connection() {
    let server = test_start().await;
    let (mut socket, _) = connect(server.port()).await;

    socket
        .send(ws_lib::Message::Text("{not json".into()))
        .await
        .unwrap();
    let response = next_json(&mut socket).await;
    assert_eq!(response["error_code"], json!(8));

    // The connection is still usable.
    let response = request(&mut socket, "1", "server_info", json!({})).await;
    assert!(response["result"]["schema_version"].is_number());
}

// -- HTTP endpoints ---------------------------------------------------------

async fn http_request(port: u16, raw: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream.write_all(raw.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
    String::from_utf8_lossy(&response).to_string()
}

#[tokio::test]
async fn an_unknown_http_path_is_a_404() {
    let server = test_start().await;
    let response = http_request(
        server.port(),
        "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 404 Not Found"),
        "{}",
        response
    );
}

#[tokio::test]
async fn the_ota_endpoint_rejects_other_methods() {
    let server = test_start().await;
    let response = http_request(
        server.port(),
        "GET /ota-upload/3f9a1c2e8b7d4a10f6c9e0b2d4a17853 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 405 Method Not Allowed"),
        "{}",
        response
    );
    assert!(response.contains("Allow: POST"));
}

#[tokio::test]
async fn an_unreserved_upload_id_is_rejected() {
    let server = test_start().await;
    let response = http_request(
        server.port(),
        "POST /ota-upload/3f9a1c2e8b7d4a10f6c9e0b2d4a17853 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabc",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request"),
        "{}",
        response
    );
    assert!(response.contains("\"error_code\":101"));
}

#[tokio::test]
async fn a_malformed_upload_path_is_a_404() {
    let server = test_start().await;
    let response = http_request(
        server.port(),
        "POST /ota-upload/short HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 404 Not Found"),
        "{}",
        response
    );
}
