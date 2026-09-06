//! Server identity, diagnostics, log levels, credentials, and the fabric label.

use serde_json::{json, Value};

use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::events::Event;
use crate::protocol::message::Args;
use crate::protocol::model::{ServerInfo, MIN_SUPPORTED_SCHEMA_VERSION, SCHEMA_VERSION};
use crate::storage::config::ConfigStore;
use crate::storage::thread_dataset;

use super::{CallContext, ServerContext};

/// Log levels the protocol reports, plus the matter.js aliases it accepts.
const LOG_LEVELS: &[(&str, &str)] = &[
    ("critical", "critical"),
    ("fatal", "critical"),
    ("error", "error"),
    ("warning", "warning"),
    ("warn", "warning"),
    ("notice", "notice"),
    ("info", "info"),
    ("debug", "debug"),
];

fn canonical_log_level(level: &str) -> Option<&'static str> {
    LOG_LEVELS
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(level))
        .map(|(_, canonical)| *canonical)
}

/// Build the `server_info` payload.
///
/// Shared with the connection handler, which sends this unsolicited as the
/// first frame, and with the `server_info_updated` event.
pub async fn build_server_info(context: &ServerContext) -> Result<ServerInfo, ApiError> {
    let fabric = context.fabric_info().await?;
    Ok(ServerInfo {
        fabric_id: fabric.fabric_id,
        compressed_fabric_id: fabric.compressed_fabric_id,
        fabric_index: Some(fabric.fabric_index),
        schema_version: SCHEMA_VERSION,
        min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
        sdk_version: context.runtime.sdk_version.clone(),
        wifi_credentials_set: context.config.wifi_credentials_set(),
        wifi_ssid: context.config.default_wifi_ssid(),
        thread_credentials_set: context.config.thread_credentials_set(),
        bluetooth_enabled: context.runtime.bluetooth_enabled,
        ble_proxy_enabled: Some(context.runtime.ble_proxy_enabled),
        controller_node_id: Some(fabric.node_id),
    })
}

/// Publish `server_info_updated` after a change that clients mirror.
async fn broadcast_server_info(context: &ServerContext) {
    match build_server_info(context).await {
        Ok(info) => context.events.publish(Event::server_info_updated(&info)),
        // A failure here means the fabric is unavailable; the caller's own
        // command already succeeded, so this must not turn into an error.
        Err(error) => log::warn!("Could not broadcast server_info: {}", error),
    }
}

pub async fn server_info(_args: &Args, context: CallContext<'_>) -> ApiResult {
    let info = build_server_info(context.server).await?;
    Ok(serde_json::to_value(info).unwrap_or(Value::Null))
}

pub async fn diagnostics(args: &Args, context: CallContext<'_>) -> ApiResult {
    let info = build_server_info(context.server).await?;
    let only_available = args.bool_or("only_available", false)?;
    Ok(json!({
        "info": info,
        "nodes": context.server.nodes.all_filtered(only_available),
        "events": context.server.event_history(),
    }))
}

pub async fn get_loglevel(_args: &Args, context: CallContext<'_>) -> ApiResult {
    Ok(json!({
        "console_loglevel": context.server.console_loglevel(),
        "file_loglevel": context.server.file_loglevel(),
    }))
}

/// Change the log levels for the lifetime of the process.
pub async fn set_loglevel(args: &Args, context: CallContext<'_>) -> ApiResult {
    // Both arguments are optional; only the ones supplied change.
    if let Some(level) = args.str("console_loglevel")? {
        let canonical = canonical_log_level(level)
            .ok_or_else(|| ApiError::invalid_args(format!("Invalid log level '{}'", level)))?;
        context.server.set_console_loglevel(canonical);
        apply_console_log_level(canonical);
    }
    if let Some(level) = args.str("file_loglevel")? {
        let canonical = canonical_log_level(level)
            .ok_or_else(|| ApiError::invalid_args(format!("Invalid log level '{}'", level)))?;
        context.server.set_file_loglevel(canonical);
    }
    get_loglevel(args, context).await
}

/// Map a protocol level onto the `log` crate's filter.
///
/// The protocol has `critical` and `notice`, which `log` does not; they fold
/// onto the nearest neighbour rather than being rejected.
fn apply_console_log_level(level: &str) {
    let filter = match level {
        "critical" | "error" => log::LevelFilter::Error,
        "warning" => log::LevelFilter::Warn,
        "notice" | "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        _ => log::LevelFilter::Info,
    };
    log::set_max_level(filter);
}

pub async fn set_wifi_credentials(args: &Args, context: CallContext<'_>) -> ApiResult {
    let ssid = args.req_str("ssid")?;
    // The password may be omitted, but only to keep the stored one for an
    // unchanged SSID; the storage layer enforces that rule.
    let credentials = args.str("credentials")?.or(args.str("password")?);
    let id = args.str("id")?;

    context
        .server
        .config
        .set_wifi_credentials(id, ssid, credentials)
        .map_err(ApiError::invalid_args)?;
    broadcast_server_info(context.server).await;
    Ok(json!({}))
}

pub async fn set_thread_dataset(args: &Args, context: CallContext<'_>) -> ApiResult {
    let dataset = args.req_str("dataset")?;
    if !thread_dataset::is_valid_hex(dataset) {
        return Err(ApiError::invalid_args(
            "Invalid Thread operational dataset: must be a non-empty hex string with even length (each byte is two hex characters)",
        ));
    }
    let id = args.str("id")?;
    context
        .server
        .config
        .set_thread_dataset(id, dataset)
        .map_err(ApiError::invalid_args)?;
    broadcast_server_info(context.server).await;
    Ok(json!({}))
}

pub async fn remove_wifi_credentials(args: &Args, context: CallContext<'_>) -> ApiResult {
    let id = args.str("id")?;
    context
        .server
        .config
        .remove_wifi_credentials(id)
        .map_err(|e| ApiError::sdk(format!("Failed to persist the credential removal: {}", e)))?;
    broadcast_server_info(context.server).await;
    Ok(json!({}))
}

pub async fn remove_thread_dataset(args: &Args, context: CallContext<'_>) -> ApiResult {
    let id = args.str("id")?;
    context
        .server
        .config
        .remove_thread_dataset(id)
        .map_err(|e| ApiError::sdk(format!("Failed to persist the credential removal: {}", e)))?;
    broadcast_server_info(context.server).await;
    Ok(json!({}))
}

pub async fn get_all_credentials(_args: &Args, context: CallContext<'_>) -> ApiResult {
    Ok(serde_json::to_value(context.server.config.summaries()).unwrap_or(Value::Null))
}

/// Set the label this controller presents on fabrics it joins.
///
/// Two cases succeed without changing anything, matching the reference: a
/// label pinned at startup, and a request from a connection that does not own
/// the label. Both are logged, because a client that believes it set the label
/// otherwise has no way to tell.
pub async fn set_default_fabric_label(args: &Args, context: CallContext<'_>) -> ApiResult {
    // `null` is explicitly allowed and means "reset to the default".
    let requested = args.str("label")?;
    let label = ConfigStore::normalize_fabric_label(requested);

    if let Some(pinned) = &context.server.runtime.pinned_fabric_label {
        log::info!(
            "Ignoring set_default_fabric_label('{}'): the label is pinned to '{}'",
            label,
            pinned
        );
        return Ok(Value::Null);
    }

    if !context.server.claim_fabric_label(context.connection_id) {
        log::info!(
            "Ignoring set_default_fabric_label('{}') from connection {}: another connection owns the label",
            label,
            context.connection_id
        );
        return Ok(Value::Null);
    }

    context.server.matter.set_fabric_label(&label).await?;
    context
        .server
        .config
        .set_fabric_label(&label)
        .map_err(|e| ApiError::sdk(format!("Failed to persist the fabric label: {}", e)))?;
    context.server.update_cached_fabric_label(&label);

    // Existing nodes were told the old label at commissioning; refresh them so
    // what other ecosystems display matches what was just set. Unreachable
    // nodes are skipped rather than failing the request.
    for node in context.server.nodes.all_filtered(true) {
        if node.is_test_node() {
            continue;
        }
        if let Err(error) = super::fabrics::push_fabric_label(context, node.node_id, &label).await {
            log::info!(
                "Node {} did not take the new fabric label: {}",
                node.node_id,
                error
            );
        }
    }

    broadcast_server_info(context.server).await;
    Ok(Value::Null)
}

pub async fn get_fabric_label(_args: &Args, context: CallContext<'_>) -> ApiResult {
    let label = match &context.server.runtime.pinned_fabric_label {
        Some(pinned) => pinned.clone(),
        None => context.server.config.fabric_label(),
    };
    Ok(json!({ "fabric_label": label }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::{call, test_context, test_context_with_fabric};
    use futures_lite::future::block_on;

    #[test]
    fn server_info_reports_the_real_fabric_and_schema() {
        let context = test_context_with_fabric();
        let result = block_on(server_info(&Args::default(), call(&context))).unwrap();
        assert_eq!(result["fabric_id"], json!(1));
        assert_eq!(
            result["compressed_fabric_id"],
            json!(0x1234_5678_9ABC_DEF0u64)
        );
        assert_eq!(result["fabric_index"], json!(1));
        assert_eq!(result["schema_version"], json!(13));
        assert_eq!(result["min_supported_schema_version"], json!(11));
        assert_eq!(result["controller_node_id"], json!(112233));
        assert_eq!(result["wifi_credentials_set"], json!(false));
        assert_eq!(result["thread_credentials_set"], json!(false));
        // An unset SSID is omitted rather than sent as null.
        assert!(result.get("wifi_ssid").is_none());
    }

    #[test]
    fn diagnostics_uses_the_documented_key_names() {
        let context = test_context_with_fabric();
        let result = block_on(diagnostics(&Args::default(), call(&context))).unwrap();
        assert!(result.get("info").is_some());
        assert_eq!(result["nodes"], json!([]));
        assert_eq!(result["events"], json!([]));
    }

    #[test]
    fn log_levels_accept_the_matter_js_aliases() {
        let context = test_context();
        let args = Args::new(json!({ "console_loglevel": "warn" }));
        let result = block_on(set_loglevel(&args, call(&context))).unwrap();
        assert_eq!(result["console_loglevel"], json!("warning"));
        assert_eq!(result["file_loglevel"], Value::Null);

        let args = Args::new(json!({ "console_loglevel": "fatal" }));
        let result = block_on(set_loglevel(&args, call(&context))).unwrap();
        assert_eq!(result["console_loglevel"], json!("critical"));
    }

    #[test]
    fn an_unknown_log_level_is_an_argument_error() {
        let context = test_context();
        let args = Args::new(json!({ "console_loglevel": "loud" }));
        let error = block_on(set_loglevel(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
    }

    #[test]
    fn setting_wifi_credentials_updates_server_info() {
        let context = test_context_with_fabric();
        let args = Args::new(json!({ "ssid": "home", "credentials": "secret" }));
        assert_eq!(
            block_on(set_wifi_credentials(&args, call(&context))).unwrap(),
            json!({})
        );
        let info = block_on(server_info(&Args::default(), call(&context))).unwrap();
        assert_eq!(info["wifi_credentials_set"], json!(true));
        assert_eq!(info["wifi_ssid"], json!("home"));
    }

    #[test]
    fn a_thread_dataset_must_be_hex() {
        let context = test_context_with_fabric();
        let args = Args::new(json!({ "dataset": "nothex" }));
        let error = block_on(set_thread_dataset(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 8);
        assert!(error.details.contains("hex string"));
    }

    #[test]
    fn credentials_are_listed_without_secrets() {
        let context = test_context_with_fabric();
        let args = Args::new(json!({ "ssid": "home", "credentials": "secret" }));
        block_on(set_wifi_credentials(&args, call(&context))).unwrap();
        let listed = block_on(get_all_credentials(&Args::default(), call(&context))).unwrap();
        assert_eq!(listed["wifi"][0]["ssid"], json!("home"));
        assert!(!listed.to_string().contains("secret"));
        assert_eq!(listed["thread"][0]["id"], json!("default"));
    }

    #[test]
    fn the_fabric_label_defaults_and_is_owned_by_one_connection() {
        let context = test_context_with_fabric();
        assert_eq!(
            block_on(get_fabric_label(&Args::default(), call(&context))).unwrap(),
            json!({ "fabric_label": "HomeAssistant" })
        );

        // A second connection's request is accepted and ignored.
        let second = CallContext {
            server: &context,
            connection_id: 2,
            peer: "127.0.0.1:2",
        };
        assert!(context.claim_fabric_label(1));
        let args = Args::new(json!({ "label": "Other" }));
        assert_eq!(
            block_on(set_default_fabric_label(&args, second)).unwrap(),
            Value::Null
        );
        assert_eq!(context.config.fabric_label(), "HomeAssistant");
    }

    #[test]
    fn a_pinned_fabric_label_wins() {
        let mut context = test_context_with_fabric();
        context.runtime.pinned_fabric_label = Some("Pinned".to_string());
        let args = Args::new(json!({ "label": "Other" }));
        assert_eq!(
            block_on(set_default_fabric_label(&args, call(&context))).unwrap(),
            Value::Null
        );
        assert_eq!(
            block_on(get_fabric_label(&Args::default(), call(&context))).unwrap(),
            json!({ "fabric_label": "Pinned" })
        );
    }
}
