//! A minimal stdio MCP transport over the broker control-plane seam.

use std::io::{BufRead, Write};

use fpsmaxxing_contracts::ChangeRequest;
use fpsmaxxing_control_plane::{ControlPlane, ControlPlaneError};
use serde_json::{Value, json};
use thiserror::Error;

/// Errors returned while accepting MCP JSON-RPC messages.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// The broker or journal rejected a request.
    #[error(transparent)]
    ControlPlane(#[from] ControlPlaneError),
    /// A transport reader or writer failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A message was not valid JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Serves line-delimited JSON-RPC MCP requests until the client closes stdin.
///
/// Malformed JSON receives a parse-error response and the session continues;
/// notifications (messages without an `id`) never receive a response.
///
/// # Errors
///
/// Returns an error for a failed transport reader or writer.
pub fn serve(
    input: impl BufRead,
    mut output: impl Write,
    mut plane: ControlPlane,
) -> Result<(), GatewayError> {
    for line in input.lines() {
        let response = match serde_json::from_str::<Value>(&line?) {
            Ok(request) => dispatch(&mut plane, &request),
            Err(error) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("parse error: {error}") }
            })),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn dispatch(plane: &mut ControlPlane, request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let result = match request.get("method").and_then(Value::as_str) {
        Some("initialize") => Ok(json!({
            "protocolVersion": "2025-03-26",
            "serverInfo": { "name": "fpsmaxxing-gateway", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "tools": {} }
        })),
        Some("tools/list") => Ok(json!({ "tools": [
            { "name": "fpsmaxxing.capabilities", "description": "Discover typed, policy-approved capabilities", "inputSchema": { "type": "object", "additionalProperties": false } },
            { "name": "fpsmaxxing.run_mock_lifecycle", "description": "Run snapshot, preview, apply, verify, and rollback for mock.value", "inputSchema": {
                "type": "object", "additionalProperties": false, "required": ["value", "lease_seconds"],
                "properties": { "value": { "type": "integer", "minimum": 0, "maximum": 100 }, "lease_seconds": { "type": "integer", "minimum": 1, "maximum": 300 } }
            }}
        ]})),
        Some("tools/call") => call_tool(plane, request.get("params").unwrap_or(&Value::Null)),
        Some(method) => Err((-32601, format!("method not found: {method}"))),
        None => Err((-32600, "missing MCP method".to_owned())),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

fn call_tool(plane: &mut ControlPlane, params: &Value) -> Result<Value, (i64, String)> {
    match params.get("name").and_then(Value::as_str) {
        Some("fpsmaxxing.capabilities") => Ok(success(plane.capabilities())),
        Some("fpsmaxxing.run_mock_lifecycle") => {
            let arguments = params
                .get("arguments")
                .ok_or_else(|| (-32602, "missing tool arguments".to_owned()))?;
            let request = serde_json::from_value::<ChangeRequest>(json!({
                "capability_id": "mock.value", "parameters": { "value": arguments.get("value").cloned().unwrap_or(Value::Null) },
                "lease_seconds": arguments.get("lease_seconds").cloned().unwrap_or(Value::Null)
            }))
            .map_err(|error| (-32602, format!("invalid lifecycle arguments: {error}")))?;
            Ok(match plane.run_lifecycle(&request) {
                Ok(result) => success(result),
                Err(error) => failure(&error.to_string()),
            })
        }
        Some(name) => Err((-32602, format!("unknown tool: {name}"))),
        None => Err((-32602, "missing tool name".to_owned())),
    }
}

fn success(value: impl serde::Serialize) -> Value {
    match serde_json::to_string(&value) {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
        Err(error) => failure(&format!("serialization failed: {error}")),
    }
}

fn failure(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}
