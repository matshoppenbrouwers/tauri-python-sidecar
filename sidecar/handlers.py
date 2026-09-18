"""The template's three demo handlers.

This is where the ~10,400 lines of application logic used to live. What matters
is the shape: one class per method, registered into the dispatcher, and the
renderer-side allowlist in `src-tauri/src/commands/sidecar.rs` naming exactly
the same three methods. Adding a method means touching both sides — that is
deliberate, it keeps the renderer from reaching arbitrary sidecar internals.
"""

from __future__ import annotations

import os
from typing import Any

from sidecar.config import load_config
from sidecar.dispatcher import CommandHandler, JsonRpcDispatcher
from sidecar.storage.schema import CURRENT_SCHEMA_VERSION, open_database


class PingHandler(CommandHandler):
    """Liveness probe. The Rust health check calls this after every spawn."""

    def get_method_name(self) -> str:
        return "ping"

    def execute(self, params: dict[str, Any]) -> dict[str, Any]:
        return {"pong": True, "pid": os.getpid()}


class EchoHandler(CommandHandler):
    """Round-trips a string, so the UI can prove the transport really works."""

    def get_method_name(self) -> str:
        return "echo"

    def execute(self, params: dict[str, Any]) -> dict[str, Any]:
        message = params.get("message", "")
        if not isinstance(message, str):
            raise ValueError("message must be a string")
        return {"message": message}


class GetStatusHandler(CommandHandler):
    """Reports process identity and the applied schema version.

    The schema version is what the demo UI shows: it is proof that the
    migration runner ran in this process, against the real database file.
    """

    def get_method_name(self) -> str:
        return "get_status"

    def execute(self, params: dict[str, Any]) -> dict[str, Any]:
        config = load_config()
        conn = open_database(config.db_path)
        try:
            (note_count,) = conn.execute("SELECT COUNT(*) FROM notes").fetchone()
        finally:
            conn.close()

        return {
            "pid": os.getpid(),
            "schema_version": CURRENT_SCHEMA_VERSION,
            "note_count": note_count,
        }


def register_all_handlers(dispatcher: JsonRpcDispatcher) -> None:
    """Register every handler this sidecar serves."""
    for handler in (PingHandler(), EchoHandler(), GetStatusHandler()):
        dispatcher.register(handler)
