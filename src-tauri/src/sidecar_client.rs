//! TCP client for persistent sidecar server.
//!
//! Provides simple TCP communication to the sidecar server (port 9124).
//! Creates a fresh connection per request for reliability.
//!
//! Supports two modes:
//! - Simple: Single request, single response line
//! - Streaming: Progress events streamed before final response

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::paths;

/// Default sidecar server address
const SIDECAR_HOST: &str = "127.0.0.1";
const SIDECAR_PORT: u16 = 9124;

/// Connection timeout for TCP socket
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Read timeout for responses (simple mode)
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Read timeout for streaming mode (longer for query operations)
const READ_TIMEOUT_STREAMING: Duration = Duration::from_secs(120);
/// Write timeout for requests
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Error prefix for connection failures (server unreachable).
/// Used by sidecar.rs to distinguish "server down" (safe to spawn fallback)
/// from "server busy" (do NOT spawn — would cause double execution).
pub const CONNECT_ERROR_PREFIX: &str = "[connect] ";

/// Parse the default sidecar address.
fn default_addr() -> Result<std::net::SocketAddr, String> {
    format!("{}:{}", SIDECAR_HOST, SIDECAR_PORT)
        .parse()
        .map_err(|e| format!("Invalid address: {}", e))
}

/// Read the sidecar session token written by the Python server (0600 file).
///
/// Returns `None` when the file is absent (e.g. a server running without auth),
/// in which case no handshake line is sent.
fn read_sidecar_token() -> Option<String> {
    let token_path = paths::get_index_dir().join("sidecar_server.token");
    std::fs::read_to_string(token_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write the auth handshake line (`{"type":"auth","token":"..."}`) when a token
/// is available. The server consumes this first line before the JSON-RPC request;
/// on success it stays silent so the one-response-line contract is preserved.
fn write_auth_handshake(stream: &mut TcpStream) -> Result<(), String> {
    if let Some(token) = read_sidecar_token() {
        let auth_line = format!(
            "{}\n",
            serde_json::json!({ "type": "auth", "token": token })
        );
        stream
            .write_all(auth_line.as_bytes())
            .map_err(|e| format!("Failed to send auth handshake: {}", e))?;
    }
    Ok(())
}

/// Core TCP request logic shared by `send_request` and `send_request_long`.
///
/// Creates a fresh TCP connection, sends the request, and reads a single
/// newline-terminated response line.
fn send_request_inner(
    request: &str,
    addr: std::net::SocketAddr,
    read_timeout: Duration,
) -> Result<String, String> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("{CONNECT_ERROR_PREFIX}Failed to connect to sidecar: {e}"))?;

    // Set timeouts
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|e| format!("Failed to set read timeout: {}", e))?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| format!("Failed to set write timeout: {}", e))?;

    // Disable Nagle's algorithm for lower latency
    stream
        .set_nodelay(true)
        .map_err(|e| format!("Failed to set nodelay: {}", e))?;

    // Authenticate before sending the request (no-op when no token file exists)
    write_auth_handshake(&mut stream)?;

    // Send request (newline-terminated)
    let request_line = if request.ends_with('\n') {
        request.to_string()
    } else {
        format!("{}\n", request)
    };

    stream
        .write_all(request_line.as_bytes())
        .map_err(|e| format!("Failed to send request: {}", e))?;

    stream
        .flush()
        .map_err(|e| format!("Failed to flush: {}", e))?;

    // Read response (newline-terminated)
    let mut reader = BufReader::new(&stream);
    let mut response = String::new();

    reader
        .read_line(&mut response)
        .map_err(|e| format!("Failed to read response: {}", e))?;

    if response.is_empty() {
        return Err("Empty response from sidecar".to_string());
    }

    Ok(response.trim_end().to_string())
}

/// Send a JSON-RPC request to the persistent sidecar server.
///
/// Creates a fresh TCP connection for each request to avoid buffering issues.
/// This is still fast (~1-5ms) compared to process spawning (~50-100ms).
///
/// # Arguments
/// * `request` - JSON-RPC request as a string (will be newline-terminated)
///
/// # Returns
/// * `Ok(String)` - JSON-RPC response
/// * `Err(String)` - Error message if connection or communication failed
pub fn send_request(request: &str) -> Result<String, String> {
    send_request_inner(request, default_addr()?, READ_TIMEOUT)
}

/// Send a JSON-RPC request with a longer read timeout (120s).
///
/// Identical to `send_request` but uses `READ_TIMEOUT_STREAMING` for operations
/// that take longer than 30s (e.g., task execution via the scheduler).
/// Does NOT support progress events — use `send_request_with_progress` for that.
pub fn send_request_long(request: &str) -> Result<String, String> {
    send_request_inner(request, default_addr()?, READ_TIMEOUT_STREAMING)
}

/// Send a JSON-RPC request with streaming progress event support.
///
/// Reads multiple lines from the TCP connection:
/// - Lines with `"type": "progress"` are emitted as Tauri events
/// - The final line (JSON-RPC response with "id" field) is returned
///
/// This is used for query operations that emit progress events during execution.
///
/// # Arguments
/// * `request` - JSON-RPC request as a string (will be newline-terminated)
/// * `app` - Tauri app handle for emitting progress events
///
/// # Returns
/// * `Ok(String)` - JSON-RPC response (final line)
/// * `Err(String)` - Error message if connection or communication failed
pub fn send_request_with_progress(request: &str, app: &AppHandle) -> Result<String, String> {
    let addr = default_addr()?;

    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("{CONNECT_ERROR_PREFIX}Failed to connect to sidecar: {e}"))?;

    stream
        .set_read_timeout(Some(READ_TIMEOUT_STREAMING))
        .map_err(|e| format!("Failed to set read timeout: {}", e))?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| format!("Failed to set write timeout: {}", e))?;
    stream
        .set_nodelay(true)
        .map_err(|e| format!("Failed to set nodelay: {}", e))?;

    // Authenticate before sending the request (no-op when no token file exists)
    write_auth_handshake(&mut stream)?;

    let request_line = if request.ends_with('\n') {
        request.to_string()
    } else {
        format!("{}\n", request)
    };

    stream
        .write_all(request_line.as_bytes())
        .map_err(|e| format!("Failed to send request: {}", e))?;
    stream
        .flush()
        .map_err(|e| format!("Failed to flush: {}", e))?;

    // Read and process multiple lines (progress events + final response)
    let reader = BufReader::new(&stream);

    for line_result in reader.lines() {
        let line = line_result.map_err(|e| format!("Failed to read line: {}", e))?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        match classify_stream_line(trimmed) {
            StreamLine::Progress(json) => {
                log::debug!("TCP progress event: {:?}", json);
                let _ = app.emit("query-progress", json);
            }
            StreamLine::ProtocolEvent(json) => {
                if let Some(event) = json.get("event") {
                    let _ = app.emit("protocol-event", event);
                }
            }
            StreamLine::FinalResponse => return Ok(trimmed.to_string()),
            StreamLine::Skip => {
                // Stray log line or unrecognized JSON — never treat as the final
                // response, or a single stray line would end the read early.
                log::warn!("Skipping unrecognized sidecar stream line: {}", trimmed);
            }
        }
    }

    Err("No response received from sidecar".to_string())
}

/// Classification of a single line read from the sidecar streaming socket.
#[derive(Debug)]
enum StreamLine {
    /// Progress event to relay to the frontend (`"type": "progress"`)
    Progress(serde_json::Value),
    /// Protocol ShellEvent to relay to the frontend (`"type": "protocol_event"`)
    ProtocolEvent(serde_json::Value),
    /// Final JSON-RPC response (has `id`/`jsonrpc`/`result`/`error`)
    FinalResponse,
    /// Anything else (stray log line, unrecognized JSON) — skip, never terminate
    Skip,
}

/// Classify a trimmed, non-empty line from the streaming connection.
///
/// Only lines that parse as JSON with a JSON-RPC response shape terminate the
/// read; everything unrecognized is skipped so stray output cannot end the
/// stream early.
fn classify_stream_line(trimmed: &str) -> StreamLine {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return StreamLine::Skip;
    };

    match json.get("type").and_then(|t| t.as_str()) {
        Some("progress") => return StreamLine::Progress(json),
        Some("protocol_event") => return StreamLine::ProtocolEvent(json),
        _ => {}
    }

    if json.get("id").is_some()
        || json.get("jsonrpc").is_some()
        || json.get("result").is_some()
        || json.get("error").is_some()
    {
        StreamLine::FinalResponse
    } else {
        StreamLine::Skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_address_parsing() {
        let addr: std::net::SocketAddr = format!("{}:{}", SIDECAR_HOST, SIDECAR_PORT)
            .parse()
            .unwrap();
        assert_eq!(addr.port(), 9124);
    }

    /// Get an address with a guaranteed-closed port (bind to :0, grab the port, drop).
    fn closed_port_addr() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    #[test]
    fn test_connect_error_has_prefix() {
        // Connect to a guaranteed-closed port. sidecar.rs depends on this prefix
        // for spawn-fallback gating — if this test fails, double execution can recur.
        let addr = closed_port_addr();
        let err = send_request_inner(
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#,
            addr,
            READ_TIMEOUT,
        )
        .unwrap_err();
        assert!(
            err.starts_with(CONNECT_ERROR_PREFIX),
            "Connection error should start with prefix '{CONNECT_ERROR_PREFIX}', got: {err}"
        );
    }

    #[test]
    fn test_classify_progress_line() {
        let line = r#"{"type":"progress","stage":"embedding","pct":50}"#;
        assert!(matches!(
            classify_stream_line(line),
            StreamLine::Progress(_)
        ));
    }

    #[test]
    fn test_classify_protocol_event_line() {
        let line = r#"{"type":"protocol_event","event":{"kind":"turn_started"}}"#;
        assert!(matches!(
            classify_stream_line(line),
            StreamLine::ProtocolEvent(_)
        ));
    }

    #[test]
    fn test_classify_jsonrpc_response_is_final() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        assert!(matches!(
            classify_stream_line(line),
            StreamLine::FinalResponse
        ));
    }

    #[test]
    fn test_classify_jsonrpc_error_is_final() {
        let line = r#"{"id":1,"error":{"code":-32000,"message":"boom"}}"#;
        assert!(matches!(
            classify_stream_line(line),
            StreamLine::FinalResponse
        ));
    }

    #[test]
    fn test_classify_stray_log_line_is_skipped() {
        // A stray log line must NOT terminate the stream as a final response
        assert!(matches!(
            classify_stream_line("INFO: sidecar warming up handlers"),
            StreamLine::Skip
        ));
    }

    #[test]
    fn test_classify_unrecognized_json_is_skipped() {
        assert!(matches!(
            classify_stream_line(r#"{"foo":"bar"}"#),
            StreamLine::Skip
        ));
    }

    #[test]
    fn test_connect_error_has_prefix_long() {
        let addr = closed_port_addr();
        let err = send_request_inner(
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#,
            addr,
            READ_TIMEOUT_STREAMING,
        )
        .unwrap_err();
        assert!(
            err.starts_with(CONNECT_ERROR_PREFIX),
            "Connection error should start with prefix '{CONNECT_ERROR_PREFIX}', got: {err}"
        );
    }
}
