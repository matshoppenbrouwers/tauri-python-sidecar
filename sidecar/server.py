"""
Persistent JSON-RPC TCP server for sidecar.

Provides persistent JSON-RPC handling via TCP socket, replacing the per-request
stdin/stdout model for improved performance.

Usage:
    python -m sidecar.loader sidecar-server [--worker-name=NAME]
"""

from __future__ import annotations

import argparse
import asyncio
import hmac
import json
import logging
import os
import queue
import secrets
import signal
from pathlib import Path

from sidecar.config import SidecarConfig, load_config
from sidecar.progress import create_progress_context

logger = logging.getLogger(__name__)

# Global handles for stderr/stdout redirection cleanup
_original_stderr = None
_original_stdout = None
_log_file_stderr = None
_log_file_stdout = None


class SidecarTcpServer:
    """
    TCP server for JSON-RPC sidecar requests.

    Handles:
    - TCP connections from Tauri Rust backend
    - JSON-RPC request routing to existing handlers
    - Persistent connections for multiple requests
    - Connection lifecycle management
    """

    def __init__(self, host: str, port: int, *, require_auth: bool = True) -> None:
        """
        Initialize TCP server.

        Args:
            host: Server host address (e.g., "127.0.0.1")
            port: Server port (e.g., 9124)
            require_auth: Whether to require a per-session token handshake
                (default: True). The token is shared with the Tauri client via a
                0600 file that only this user can read.
        """
        self.host = host
        self.port = port
        self._server: asyncio.Server | None = None
        self._connections: set[asyncio.StreamWriter] = set()
        self._require_auth = require_auth
        self._token: str | None = None
        self._token_file: Path = _get_lock_file().parent / "sidecar_server.token"
        logger.info("Initialized SidecarTcpServer on %s:%d", host, port)

    def _generate_session_token(self) -> None:
        """Generate a random session token and write it with restricted permissions."""
        self._token = secrets.token_urlsafe(32)
        self._token_file.parent.mkdir(parents=True, exist_ok=True)
        # Create the file already restricted rather than writing it and calling
        # chmod after: between those two calls the token sits on disk readable by
        # every local account at the process umask, which is exactly the window an
        # attacker on a shared machine needs. O_EXCL after an unlink also refuses
        # to follow a symlink planted at this path.
        self._token_file.unlink(missing_ok=True)
        fd = os.open(
            self._token_file,
            os.O_CREAT | os.O_EXCL | os.O_WRONLY,
            0o600,
        )
        try:
            os.write(fd, self._token.encode("utf-8"))
        finally:
            os.close(fd)
        # Windows ignores the mode argument; the file inherits the ACL of the
        # per-user data directory it lives in, which is already user-scoped.
        logger.info("Sidecar session token written to %s", self._token_file)

    def _cleanup_session_token(self) -> None:
        """Delete the session token file and clear the in-memory token."""
        self._token = None
        if self._token_file.exists():
            self._token_file.unlink(missing_ok=True)
            logger.info("Sidecar session token file removed")

    async def _authenticate(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> bool:
        """Require a token handshake as the first line on a new connection.

        The client sends ``{"type": "auth", "token": "..."}`` before its first
        JSON-RPC request. On success the connection proceeds silently (no ack)
        so the one-response-line-per-request contract is preserved; on failure a
        JSON-RPC error is written and the connection is rejected.

        Returns:
            True if authenticated, False otherwise.
        """
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=5.0)
        except asyncio.TimeoutError:
            return False

        if not line:
            return False

        try:
            data = json.loads(line.decode("utf-8").strip())
        except (json.JSONDecodeError, UnicodeDecodeError):
            data = None

        received = data.get("token") if isinstance(data, dict) else None
        if (
            not isinstance(data, dict)
            or data.get("type") != "auth"
            or not self._token
            or not isinstance(received, str)
            or not hmac.compare_digest(received, self._token)
        ):
            try:
                error_response = (
                    json.dumps(
                        {
                            "jsonrpc": "2.0",
                            "id": None,
                            "error": {"code": -32000, "message": "Unauthorized"},
                        }
                    )
                    + "\n"
                )
                writer.write(error_response.encode("utf-8"))
                await writer.drain()
            except (ConnectionError, OSError):
                pass
            return False

        return True

    async def start(self) -> None:
        """
        Start TCP server.

        Binds to configured host:port and accepts connections.
        """
        if self._require_auth:
            self._generate_session_token()
        self._server = await asyncio.start_server(
            self._handle_client,
            self.host,
            self.port,
        )
        logger.info("Sidecar TCP server listening on %s:%d", self.host, self.port)

    async def stop(self) -> None:
        """Stop TCP server and close all connections."""
        self._cleanup_session_token()
        if self._server:
            self._server.close()
            await self._server.wait_closed()
            logger.info("Sidecar TCP server stopped")

        # Close all active connections
        for writer in list(self._connections):
            try:
                writer.close()
                await writer.wait_closed()
            except (ConnectionError, OSError):
                pass
        self._connections.clear()

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        """
        Handle incoming TCP connection.

        Processes multiple JSON-RPC requests on the same connection.
        Streams progress events before the final JSON-RPC response.

        Args:
            reader: Stream reader for incoming data
            writer: Stream writer for outgoing data
        """
        # Import here to avoid circular imports and for lazy loading

        loop = asyncio.get_event_loop()
        self._connections.add(writer)
        client_addr = writer.get_extra_info("peername")
        logger.info("Sidecar TCP connection opened from %s", client_addr)

        try:
            # Require a token handshake before processing any request
            if self._require_auth and not await self._authenticate(reader, writer):
                logger.warning("Rejected unauthenticated sidecar connection from %s", client_addr)
                return

            # Handle multiple requests on the same connection
            while True:
                try:
                    # Read line-delimited JSON-RPC request
                    line = await asyncio.wait_for(
                        reader.readline(),
                        timeout=300.0,  # 5 minute timeout per request
                    )

                    if not line:
                        # Client closed connection
                        break

                    request = line.decode("utf-8").strip()
                    if not request:
                        continue  # Skip empty lines

                    logger.debug("Received request from %s: %s", client_addr, request[:100])

                    # Execute request with progress streaming
                    response = await self._execute_with_progress_streaming(writer, request, loop)

                    # Send final JSON-RPC response
                    writer.write((response + "\n").encode("utf-8"))
                    await writer.drain()

                    logger.debug("Sent response to %s", client_addr)

                except asyncio.TimeoutError:
                    logger.warning("Client %s timed out", client_addr)
                    break
                except Exception as e:
                    logger.error("Error handling request from %s: %s", client_addr, e)
                    # Try to send error response
                    try:
                        error_response = (
                            json.dumps(
                                {
                                    "jsonrpc": "2.0",
                                    "id": None,
                                    "error": {
                                        "code": -32603,
                                        "message": f"Internal error: {str(e)[:200]}",
                                    },
                                }
                            )
                            + "\n"
                        )
                        writer.write(error_response.encode("utf-8"))
                        await writer.drain()
                    except (ConnectionError, OSError):
                        pass
                    break

        except Exception as e:
            logger.error("TCP connection error for %s: %s", client_addr, e, exc_info=True)
        finally:
            self._connections.discard(writer)
            try:
                writer.close()
                await writer.wait_closed()
            except (ConnectionError, OSError):
                pass
            logger.info("Sidecar TCP connection closed from %s", client_addr)

    async def _execute_with_progress_streaming(
        self,
        writer: asyncio.StreamWriter,
        request: str,
        loop: asyncio.AbstractEventLoop,
    ) -> str:
        """Execute handler while streaming progress events to client.

        Uses a thread-safe queue to bridge between the thread pool (where
        handlers run) and the async context (where we write to TCP).

        Args:
            writer: Stream writer for outgoing progress events
            request: JSON-RPC request string
            loop: Event loop for executor

        Returns:
            JSON-RPC response string
        """
        from sidecar.dispatcher import handle_jsonrpc_message_with_progress

        # Create progress context with queue
        progress_ctx, progress_queue = create_progress_context()

        # Start handler in thread pool
        handler_future = loop.run_in_executor(
            None,
            handle_jsonrpc_message_with_progress,
            request,
            progress_ctx,
        )

        # Stream progress events while handler executes
        while True:
            try:
                # Poll queue with short timeout (non-blocking in async)
                event = await asyncio.wait_for(
                    asyncio.to_thread(progress_queue.get, timeout=0.05),
                    timeout=0.1,
                )

                if event is None:
                    # Sentinel: handler signaled completion
                    break

                # Stream progress event to client
                writer.write((event + "\n").encode("utf-8"))
                await writer.drain()
                logger.debug("Streamed progress event: %s", event[:50])

            except (asyncio.TimeoutError, queue.Empty):
                # Check if handler task completed
                if handler_future.done():
                    # Drain remaining progress events with safety limit
                    drain_count = 0
                    max_drain = 100
                    while drain_count < max_drain:
                        try:
                            event = progress_queue.get_nowait()
                            if event is None:
                                break
                            writer.write((event + "\n").encode("utf-8"))
                            await writer.drain()
                            drain_count += 1
                        except queue.Empty:
                            break
                    if drain_count >= max_drain:
                        logger.warning("Progress drain hit safety limit")
                    break

        # Get final response (may raise if handler failed)
        return await asyncio.wrap_future(handler_future)


# ==================== Server Process Management ====================


def _get_lock_file() -> Path:
    """Get path to server lock file.

    This must resolve to the same directory the Rust side computes in
    `get_index_dir()`, because the session token file sits beside it and the
    client reads it from there.
    """
    index_dir = load_config().index_dir
    index_dir.mkdir(parents=True, exist_ok=True)
    return index_dir / "sidecar_server.lock"


def _running_image_name(pid: int) -> str | None:
    """Image name of the process holding `pid`, or None if there is none.

    The name matters as much as the liveness: PIDs are reused, so a live process
    at a lock file's PID may be an unrelated program that inherited the number.
    The Rust supervisor makes the same check before it kills anything.
    """
    import subprocess
    import sys

    if sys.platform == "win32":
        try:
            result = subprocess.run(
                ["tasklist", "/FI", f"PID eq {pid}", "/NH", "/FO", "CSV"],
                capture_output=True,
                text=True,
                timeout=2,
                creationflags=0x08000000,  # CREATE_NO_WINDOW
            )
        except (OSError, subprocess.TimeoutExpired):
            return None
        # Row shape: "image.exe","1234","Console","1","12,345 K".
        # Parse the fields rather than substring-matching the PID: a bare
        # `str(pid) in stdout` also matches PID 1234 when looking for 123, and
        # matches the memory column "12,312 K".
        for line in result.stdout.splitlines():
            fields = [f.strip('"') for f in line.split('","')]
            if len(fields) >= 2 and fields[1].strip('"').isdigit():
                if int(fields[1].strip('"')) == pid:
                    return fields[0].lstrip('"')
        return None

    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError, OSError):
        return None
    return Path(sys.executable).name


def _is_server_running() -> bool:
    """Check if a server owning the lock file is already running."""
    lock_file = _get_lock_file()
    if not lock_file.exists():
        return False

    try:
        lines = lock_file.read_text().splitlines()
        pid = int(lines[0].strip())
        recorded_image = lines[1].strip() if len(lines) > 1 else None
    except (ValueError, IndexError, OSError):
        lock_file.unlink(missing_ok=True)
        return False

    actual_image = _running_image_name(pid)
    if actual_image is None:
        lock_file.unlink(missing_ok=True)
        return False

    # A live PID whose image does not match is a reused PID, not our server.
    if recorded_image and actual_image.lower() != recorded_image.lower():
        logger.info(
            "Lock file PID %d is now %s, not %s; treating the lock as stale",
            pid,
            actual_image,
            recorded_image,
        )
        lock_file.unlink(missing_ok=True)
        return False

    return True


def _create_lock_file() -> None:
    """Create PID lock file for server process.

    Two lines: the PID, then this process's image name. The Rust supervisor
    compares that name against the live process before killing anything, so a
    reused PID cannot make it terminate an unrelated program.
    """
    import sys

    lock_file = _get_lock_file()
    lock_file.write_text(f"{os.getpid()}\n{Path(sys.executable).name}\n")
    logger.info("Created server lock file: %s", lock_file)


def _remove_lock_file() -> None:
    """Remove PID lock file."""
    lock_file = _get_lock_file()
    lock_file.unlink(missing_ok=True)
    logger.info("Removed server lock file")


def _setup_signal_handlers(shutdown_event: asyncio.Event) -> None:
    """Set up Unix signal handlers for graceful shutdown."""
    if os.name == "nt":
        return  # Windows doesn't support add_signal_handler

    def signal_handler():
        logger.info("Received shutdown signal")
        shutdown_event.set()

    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, signal_handler)


async def _run_shutdown_loop(shutdown_event: asyncio.Event) -> None:
    """Poll for shutdown signals and flag file."""
    from sidecar.shutdown import should_shutdown

    while not shutdown_event.is_set():
        if should_shutdown():
            logger.info("Shutdown flag detected")
            break
        try:
            await asyncio.wait_for(shutdown_event.wait(), timeout=0.5)
            break
        except asyncio.TimeoutError:
            continue


def _cleanup_file_handles() -> None:
    """Flush and close redirected file handles."""
    import sys

    try:
        if hasattr(sys.stderr, "flush"):
            sys.stderr.flush()
        if hasattr(sys.stdout, "flush"):
            sys.stdout.flush()
    except Exception:
        pass

    try:
        global _log_file_stderr, _log_file_stdout
        if "_log_file_stderr" in globals() and _log_file_stderr:
            _log_file_stderr.close()
        if "_log_file_stdout" in globals() and _log_file_stdout:
            _log_file_stdout.close()
    except Exception:
        pass


def _init_storage(config: SidecarConfig) -> None:
    """Open the database once at startup so migrations apply under the lock."""
    from sidecar.storage.schema import open_database

    open_database(config.db_path).close()


async def run_server() -> None:
    """
    Run sidecar TCP server.

    Main entry point for server process.
    Cross-platform shutdown support via flag file polling (Windows + Unix).
    """
    if _is_server_running():
        logger.warning("Sidecar server already running")
        return

    _create_lock_file()
    config = load_config()

    # Apply migrations once, before the socket opens. Doing it here rather than
    # lazily in a handler means a schema failure kills the process while the
    # supervisor is still watching the spawn, instead of surfacing as a confusing
    # error on the user's first request.
    _init_storage(config)

    server = SidecarTcpServer(
        host=config.host,
        port=config.port,
    )

    shutdown_event = asyncio.Event()
    _setup_signal_handlers(shutdown_event)

    try:
        await server.start()
        await _run_shutdown_loop(shutdown_event)
    except KeyboardInterrupt:
        logger.info("Server interrupted by user")
    except Exception as e:
        logger.error("Server error: %s", e, exc_info=True)
    finally:
        await server.stop()
        _remove_lock_file()
        _cleanup_file_handles()


def main() -> None:
    """CLI entry point for starting the server."""
    parser = argparse.ArgumentParser(description="Sidecar TCP server process")
    parser.add_argument(
        "--worker-name",
        default="sidecar_server",
        help="Worker identifier for IT process identification",
    )
    parser.parse_known_args()  # Ignore mode arg passed by loader.py

    # Configure file-only logging (no console to prevent Windows popups)
    # Workers run as detached processes with CREATE_NO_WINDOW flag
    # Writing to stderr causes Windows to briefly create a console window
    config = load_config()
    log_file = config.index_dir / "sidecar_server.log"
    log_file.parent.mkdir(parents=True, exist_ok=True)

    file_handler = logging.FileHandler(log_file, encoding="utf-8")
    file_handler.setFormatter(
        logging.Formatter("%(asctime)s - %(name)s - %(levelname)s - %(message)s")
    )

    # Read log level from config (default to INFO)
    log_level = getattr(logging, config.log_level.upper(), logging.INFO)

    root_logger = logging.getLogger()
    root_logger.setLevel(log_level)
    root_logger.addHandler(file_handler)

    # Redirect stderr/stdout to log file to catch unhandled exceptions
    import sys

    # Save original handles (stored as module-level globals)
    global _original_stderr, _original_stdout, _log_file_stderr, _log_file_stdout
    _original_stderr = sys.stderr
    _original_stdout = sys.stdout

    try:
        _log_file_stderr = open(log_file, "a", encoding="utf-8", buffering=1)
        _log_file_stdout = open(log_file, "a", encoding="utf-8", buffering=1)
        sys.stderr = _log_file_stderr
        sys.stdout = _log_file_stdout
        logger.debug("stderr and stdout redirected to log file")
    except Exception as redirect_err:
        logger.warning(f"Failed to redirect stderr/stdout: {redirect_err}")

    try:
        asyncio.run(run_server())
    except KeyboardInterrupt:
        logger.info("Server stopped by user")


if __name__ == "__main__":
    main()


__all__ = ["SidecarTcpServer", "run_server", "main"]
