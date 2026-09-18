"""Python sidecar process for a Tauri desktop app.

The Rust supervisor spawns this package as a separate OS process and speaks
line-delimited JSON-RPC 2.0 to it over an authenticated loopback TCP socket.
See `sidecar.loader` for the entry point the supervisor actually launches.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"
