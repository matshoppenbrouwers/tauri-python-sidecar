use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use tauri::{AppHandle, Emitter};

use crate::paths;
use crate::sidecar_client;

/// Explicit allowlist of JSON-RPC methods the renderer may invoke via
/// `sidecar_request`. Any method not in this list is rejected before it can
/// reach the Python backend, so an injected script in a webview cannot drive
/// arbitrary sidecar handlers (defense-in-depth alongside the TCP auth token).
///
/// This mirrors the dispatcher's registered handlers. Keep it in sync when
/// adding renderer-facing handlers — in the app this was extracted from, a test
/// read both sides and failed the build when they drifted apart. That test is
/// worth writing once your handler set is bigger than three entries.
const ALLOWED_METHODS: &[&str] = &["ping", "echo", "get_status"];

/// Methods that are routed through the streaming TCP client with the long read
/// timeout instead of the ordinary one.
///
/// The distinction matters: the ordinary client's read timeout assumes a method
/// answers in milliseconds. Anything that waits on a human (a browser OAuth
/// round-trip) or on a model call will blow through it and look like a dead
/// server. Those methods go here, and the streaming client also forwards the
/// sidecar's `progress` events to the renderer while they run.
///
/// The template's three demo handlers are all instantaneous, so this list is
/// empty. Add your slow methods to it rather than raising the global timeout.
const LONG_RUNNING_METHODS: &[&str] = &[];

/// Whether the renderer is permitted to invoke the given sidecar method.
fn is_method_allowed(method: &str) -> bool {
    ALLOWED_METHODS.contains(&method)
}

/// Command: Send JSON-RPC request to Python sidecar
///
/// Uses a two-tier approach:
/// 1. Try persistent TCP server first (fast, ~1-5ms)
/// 2. Fall back to spawning per-request process if TCP unavailable (~50-100ms)
///
/// Long-running methods use the streaming TCP client to receive progress events
#[tauri::command]
pub async fn sidecar_request(
    app: AppHandle,
    method: String,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Reject any method outside the renderer allowlist before touching the backend.
    if !is_method_allowed(&method) {
        log::warn!("Rejected disallowed sidecar method from renderer: {}", method);
        return Err(format!("Method not allowed: {}", method));
    }

    // Build JSON-RPC request
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    let request_str = serde_json::to_string(&request).map_err(|e| e.to_string())?;

    let needs_long_timeout = LONG_RUNNING_METHODS.contains(&method.as_str());

    // Try TCP first, capture result for unified post-processing.
    // Only fall back to spawn when the server is unreachable (connect error).
    // Read timeouts mean the server IS processing — spawning would cause double execution.
    let tcp_result = if needs_long_timeout {
        match sidecar_client::send_request_with_progress(&request_str, &app) {
            Ok(response_str) => {
                log::debug!("Received streaming response via TCP for method={}", method);
                Some(parse_jsonrpc_response(&response_str))
            }
            Err(e) if e.starts_with(sidecar_client::CONNECT_ERROR_PREFIX) => {
                log::debug!("TCP sidecar unavailable ({}), falling back to spawn", e);
                None
            }
            Err(e) => {
                log::warn!("TCP request failed (no spawn fallback): {}", e);
                return Err(e);
            }
        }
    } else {
        match sidecar_client::send_request(&request_str) {
            Ok(response_str) => {
                log::debug!("Received response via TCP for method={}", method);
                Some(parse_jsonrpc_response(&response_str))
            }
            Err(e) if e.starts_with(sidecar_client::CONNECT_ERROR_PREFIX) => {
                log::debug!("TCP sidecar unavailable ({}), falling back to spawn", e);
                None
            }
            Err(e) => {
                log::warn!("TCP request failed (no spawn fallback): {}", e);
                return Err(e);
            }
        }
    };

    // Use TCP result or fall back to spawn
    match tcp_result {
        Some(r) => r,
        None => sidecar_request_spawn(app.clone(), &request_str, &method).await,
    }
}

/// Parse JSON-RPC response and extract result or error
fn parse_jsonrpc_response(response_str: &str) -> Result<serde_json::Value, String> {
    let response: serde_json::Value = serde_json::from_str(response_str)
        .map_err(|e| format!("Failed to parse JSON response: {}", e))?;

    if let Some(error) = response.get("error") {
        if !error.is_null() {
            return Err(serde_json::to_string(&error).unwrap_or_else(|_| format!("{}", error)));
        }
    }

    response
        .get("result")
        .cloned()
        .ok_or_else(|| "Missing result in JSON-RPC response".to_string())
}

/// Fallback: Spawn a new sidecar process for this request
///
/// This is the original per-request behavior, kept as fallback when TCP server
/// is unavailable. It runs the loader's one-shot `sidecar` mode, which reads a
/// single JSON-RPC line on stdin and writes the response on stdout. Keeping
/// both paths is what makes the app usable while the supervised server is still
/// starting or is mid-restart after a crash.
async fn sidecar_request_spawn(
    app: AppHandle,
    request_str: &str,
    method: &str,
) -> Result<serde_json::Value, String> {
    let request_line = format!("{}\n", request_str);

    let config = paths::get_sidecar_config()?;
    let mut cmd = config.command_for_mode("sidecar");

    // Same data directory as the supervised server, for the same reason: this
    // one-shot process opens the same database and must not guess its location.
    cmd.env("SIDECAR_DATA_DIR", paths::get_data_dir());

    log::debug!(
        "Spawning sidecar (dev_mode={}): {:?}",
        config.is_dev,
        cmd.get_program()
    );

    // Configure stdio for JSON-RPC communication
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Spawn sidecar process with proper Windows console suppression
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn sidecar: {}", e))?;

    log::debug!("Sending JSON-RPC request: method={}", method);

    // Write request to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(request_line.as_bytes())
            .map_err(|e| format!("Failed to write to sidecar stdin: {}", e))?;
        // Drop stdin to signal EOF to child process
    } else {
        return Err("Failed to open stdin".to_string());
    }

    // Read stdout and stderr in blocking manner (we're already in async context)
    let stdout = child
        .stdout
        .take()
        .ok_or("Failed to open stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or("Failed to open stderr".to_string())?;

    // Use blocking task for stdout reading since BufReader is blocking
    let app_clone = app.clone();
    let stdout_handle = tokio::task::spawn_blocking(move || {
        let reader = BufReader::new(stdout);
        let mut stdout_lines = Vec::new();

        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    let trimmed = line.trim();
                    // Try to parse as JSON to check for progress events
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        if json.get("type").and_then(|t| t.as_str()) == Some("progress") {
                            log::debug!("Emitting progress event: {:?}", json);
                            // Emit as Tauri event to frontend
                            let _ = app_clone.emit("query-progress", json);
                            // Don't add to stdout_lines, as this is not the final response
                            continue;
                        }
                    }
                    // Regular stdout line (likely the JSON-RPC response)
                    stdout_lines.push(line);
                }
                Err(e) => {
                    log::error!("Error reading stdout: {}", e);
                    break;
                }
            }
        }
        stdout_lines
    });

    // Read stderr in blocking task
    let stderr_handle = tokio::task::spawn_blocking(move || {
        let reader = BufReader::new(stderr);
        let mut stderr_lines = Vec::new();

        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    log::debug!("Sidecar stderr: {}", line);
                    stderr_lines.push(line);
                }
                Err(e) => {
                    log::error!("Error reading stderr: {}", e);
                    break;
                }
            }
        }
        stderr_lines
    });

    // Wait for process to complete (blocking wait off the async runtime,
    // consistent with the stdout/stderr readers above)
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(|e| format!("Failed to join sidecar wait task: {}", e))?
        .map_err(|e| format!("Failed to wait for child process: {}", e))?;

    log::debug!("Sidecar process terminated with status: {:?}", status);

    // Wait for stdout and stderr reading to complete
    let stdout_lines = stdout_handle
        .await
        .map_err(|e| format!("Failed to read stdout: {}", e))?;
    let stderr_lines = stderr_handle
        .await
        .map_err(|e| format!("Failed to read stderr: {}", e))?;

    // Get first stdout line (JSON-RPC response)
    let response_str = stdout_lines.first().ok_or_else(|| {
        let stderr_str = stderr_lines.join("\n");
        log::error!("No stdout response from sidecar. Stderr: {}", stderr_str);
        format!("No response from sidecar. Stderr: {}", stderr_str)
    })?;

    log::debug!("Received JSON-RPC response");

    parse_jsonrpc_response(response_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_method_allowed_accepts_known_methods() {
        assert!(is_method_allowed("ping"));
        assert!(is_method_allowed("echo"));
        assert!(is_method_allowed("get_status"));
    }

    #[test]
    fn test_is_method_allowed_rejects_unknown_methods() {
        assert!(!is_method_allowed("definitely_not_a_method"));
        assert!(!is_method_allowed(""));
        assert!(!is_method_allowed("eval"));
        assert!(!is_method_allowed("PING")); // case-sensitive
    }

    #[test]
    fn test_parse_jsonrpc_response_extracts_result() {
        let response_str = r#"{"jsonrpc":"2.0","id":1,"result":{"echo":"abc123"}}"#;
        let result = parse_jsonrpc_response(response_str).unwrap();
        assert_eq!(result["echo"], "abc123");
    }

    #[test]
    fn test_parse_jsonrpc_response_returns_error() {
        let response_str =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid params"}}"#;
        let err = parse_jsonrpc_response(response_str).unwrap_err();
        assert!(err.contains("-32602"));
        assert!(err.contains("Invalid params"));
    }

    #[test]
    fn test_parse_jsonrpc_response_missing_result() {
        let response_str = r#"{"jsonrpc":"2.0","id":1}"#;
        let err = parse_jsonrpc_response(response_str).unwrap_err();
        assert!(err.contains("Missing result"));
    }

    #[test]
    fn test_parse_jsonrpc_response_ignores_null_error() {
        let response_str = r#"{"jsonrpc":"2.0","id":1,"error":null,"result":{"ok":true}}"#;
        let result = parse_jsonrpc_response(response_str).unwrap();
        assert_eq!(result["ok"], true);
    }
}
