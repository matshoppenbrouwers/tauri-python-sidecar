"""Unified entry point for the frozen Python sidecar executable.

This module provides a single dispatcher that routes to different modes
based on the command-line argument. When frozen with Nuitka, this becomes
the main entry point for py-sidecar.exe.

Usage (development):
    python -m sidecar.loader sidecar-server  # JSON-RPC sidecar (TCP, persistent)
    python -m sidecar.loader sidecar         # one request on stdin, one on stdout

Usage (frozen):
    py-sidecar.exe sidecar-server --worker-name=sidecar_server

The --worker-name argument is passed through for process identification
in Task Manager but doesn't affect behavior.

One binary, many modes, is the shape worth copying: a real app grows a
background worker or a sync process, and each is this same executable with a
different first argument. `_dispatch` is the only place that has to change, and
the Rust side already models it (`SidecarConfig::command_for_mode`).
"""

from __future__ import annotations

import argparse
import logging
import sys
from multiprocessing import freeze_support

logger = logging.getLogger(__name__)


def _write_crash_log(mode: str, error: BaseException) -> None:
    """Write crash details to .index/<mode>_crash.log for post-mortem diagnosis.

    In frozen GUI executables stderr is /dev/null, so unhandled exceptions
    during import are silently lost.  This function ensures the traceback
    is persisted to a known location that can be inspected later.
    """
    import traceback
    from pathlib import Path

    crash_file = Path(".index") / f"{mode}_crash.log"
    try:
        crash_file.parent.mkdir(parents=True, exist_ok=True)
        with open(crash_file, "w", encoding="utf-8") as f:
            f.write(f"Mode: {mode}\n")
            f.write(f"Error: {error}\n\n")
            traceback.print_exc(file=f)
    except Exception:
        logger.debug("Failed to write crash log %s", crash_file, exc_info=True)


def main() -> None:
    """Main entry point for all sidecar modes."""
    # CRITICAL: Required for Windows frozen processes that use multiprocessing
    # Must be called before any multiprocessing imports or usage
    freeze_support()

    # Parse mode from first positional argument
    parser = argparse.ArgumentParser(
        description="Python sidecar",
        prog="py-sidecar",
    )
    parser.add_argument(
        "mode",
        nargs="?",
        default="sidecar-server",
        choices=["sidecar-server", "sidecar"],
        help="Operating mode (default: sidecar-server)",
    )
    parser.add_argument(
        "--worker-name",
        default=None,
        help="Worker identifier for process identification (appears in Task Manager)",
    )

    args, remaining = parser.parse_known_args()

    # Dispatch to appropriate module
    # Using late imports to minimize startup time for each mode
    try:
        _dispatch(args.mode)
    except Exception as exc:
        _write_crash_log(args.mode, exc)
        raise


def _run_oneshot() -> None:
    """Answer exactly one JSON-RPC request on stdin, then exit.

    This is the other half of the dual path in
    `src-tauri/src/commands/sidecar.rs`: when the renderer makes a request and
    the supervised TCP server is unreachable — it is still starting, or it has
    just crashed and the supervisor is waiting out a backoff delay — Rust spawns
    this mode instead of failing the call. It is slower (a whole interpreter per
    request) but it keeps the app usable through a restart window rather than
    surfacing a connection error the user can do nothing about.

    One request, then exit: the caller writes a line, closes stdin and reads a
    line. Looping here would leave a process alive that nothing supervises.
    """
    from sidecar.dispatcher import handle_jsonrpc_message

    # Windows defaults these pipes to cp1252, which turns any non-ASCII payload
    # into an encoding error instead of a response.
    sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)  # type: ignore[attr-defined]
    sys.stdin.reconfigure(encoding="utf-8")  # type: ignore[attr-defined]

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        sys.stdout.write(handle_jsonrpc_message(line) + "\n")
        sys.stdout.flush()
        return


def _dispatch(mode: str) -> None:
    """Import and run the handler for *mode*."""
    if mode == "sidecar-server":
        from sidecar.server import main as sidecar_server_main

        sidecar_server_main()

    elif mode == "sidecar":
        _run_oneshot()

    else:
        print(f"Unknown mode: {mode}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
