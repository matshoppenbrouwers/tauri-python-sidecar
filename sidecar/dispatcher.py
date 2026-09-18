"""
Registry-based command dispatcher for JSON-RPC methods.

This module implements the class-based command handler pattern following SOLID
principles. Each command is implemented as a separate handler class that can
be tested and maintained independently.

It also owns the JSON-RPC 2.0 message envelope: parse, validate, dispatch,
and serialise the response. The transport (see ``server.py``) never looks
inside a message.
"""

import json
import logging
from abc import ABC, abstractmethod
from typing import Any

from sidecar.errors import (
    JsonRpcError,
    create_invalid_request_error,
    create_method_not_found_error,
    create_parse_error,
)
from sidecar.progress import ProgressContext, set_progress_context

logger = logging.getLogger(__name__)


class CommandHandler(ABC):
    """
    Base class for JSON-RPC command handlers.

    Each handler implements a single command (single responsibility principle)
    and can be tested independently.
    """

    @abstractmethod
    def execute(self, params: dict[str, Any]) -> dict[str, Any]:
        """
        Execute command with the given parameters.

        Validation is each handler's own responsibility — the dispatcher does
        not centrally validate ``params``. Handlers that accept untrusted input
        must validate it themselves.

        Args:
            params: Raw request parameters (not centrally validated).

        Returns:
            Response dictionary (will be serialized to JSON)

        Raises:
            JsonRpcError: For expected errors (validation, business logic)
            Other exceptions: Converted to JsonRpcError by dispatcher
        """
        pass

    @abstractmethod
    def get_method_name(self) -> str:
        """Return the JSON-RPC method name this handler serves."""
        pass


class JsonRpcDispatcher:
    """
    Registry-based command dispatcher for JSON-RPC methods.

    Handlers are registered on initialization and dispatched based on
    the method name in the request.
    """

    def __init__(self):
        self._handlers: dict[str, CommandHandler] = {}

    def register(self, handler: CommandHandler) -> None:
        """
        Register a command handler.

        Args:
            handler: Command handler instance

        Raises:
            ValueError: If handler is already registered for this method
        """
        method_name = handler.get_method_name()
        if method_name in self._handlers:
            raise ValueError(f"Handler already registered: {method_name}")
        self._handlers[method_name] = handler
        logger.debug("Registered JSON-RPC handler: %s", method_name)

    def dispatch(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        """
        Dispatch JSON-RPC request to appropriate handler.

        Args:
            method: JSON-RPC method name
            params: Request parameters

        Returns:
            Response dictionary

        Raises:
            JsonRpcError: For all errors (method not found, execution errors)
        """
        handler = self._handlers.get(method)
        if handler is None:
            raise create_method_not_found_error(method, list(self._handlers.keys()))

        try:
            return handler.execute(params)
        except JsonRpcError:
            raise  # Re-raise JSON-RPC errors as-is
        except Exception as e:
            # Convert unexpected errors to sanitized JSON-RPC errors
            raise JsonRpcError.from_exception(e, method) from e

    def get_available_methods(self) -> list[str]:
        """Return list of registered method names."""
        return list(self._handlers.keys())


# Global dispatcher instance (initialized in handlers.py)
_dispatcher: JsonRpcDispatcher | None = None


def get_dispatcher() -> JsonRpcDispatcher:
    """Get the global dispatcher instance (lazy initialization)."""
    global _dispatcher
    if _dispatcher is None:
        # Use local variable to ensure _dispatcher only set after successful registration
        dispatcher = JsonRpcDispatcher()
        # Import and register handlers
        from sidecar.handlers import register_all_handlers

        register_all_handlers(dispatcher)
        # Only assign to global after successful registration
        _dispatcher = dispatcher
        logger.info(
            "Dispatcher initialized with %d handlers", len(_dispatcher.get_available_methods())
        )
    return _dispatcher


def handle_request(method: str, params: dict[str, Any]) -> dict[str, Any]:
    """
    Main entry point for JSON-RPC request handling.

    Args:
        method: JSON-RPC method name
        params: Request parameters

    Returns:
        Response dictionary

    Raises:
        JsonRpcError: For all errors
    """
    return get_dispatcher().dispatch(method, params)


# ==================== JSON-RPC 2.0 message envelope ====================


def _error_response(request_id: Any, error: JsonRpcError) -> str:
    """Serialise a JSON-RPC error response.

    ``id`` falls back to 0 when the request could not be parsed far enough to
    recover one — JSON-RPC forbids omitting the field on a response.
    """
    return json.dumps(
        {
            "jsonrpc": "2.0",
            "id": request_id if request_id is not None else 0,
            "error": error.to_dict(),
        }
    )


def handle_jsonrpc_message(message: str) -> str:
    """
    Handle a single JSON-RPC message.

    Args:
        message: JSON-RPC request as string

    Returns:
        JSON-RPC response as string
    """
    request_id: int | str | None = None

    try:
        # Parse JSON
        try:
            data = json.loads(message)
        except json.JSONDecodeError as e:
            raise create_parse_error() from e

        # Validate JSON-RPC 2.0 format
        if not isinstance(data, dict):
            raise create_invalid_request_error("Request must be a JSON object")

        if data.get("jsonrpc") != "2.0":
            raise create_invalid_request_error("Missing or invalid jsonrpc field")

        if "id" not in data:
            raise create_invalid_request_error("Missing id field")

        if "method" not in data:
            raise create_invalid_request_error("Missing method field")

        # Extract request fields
        request_id = data["id"]
        method = data["method"]
        params = data.get("params", {})

        if not isinstance(method, str):
            raise create_invalid_request_error("method must be a string")
        if not isinstance(params, dict):
            raise create_invalid_request_error("params must be an object")

        # Dispatch to handler
        result = handle_request(method, params)

        return json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result})

    except JsonRpcError as e:
        # Expected errors - return as JSON-RPC error response
        return _error_response(request_id, e)

    except Exception as e:
        # Unexpected errors - log and return generic error
        logger.critical(
            "Unexpected error in sidecar: %s",
            e,
            exc_info=True,
            extra={"error_type": type(e).__name__},
        )
        return _error_response(request_id, JsonRpcError(code=-32603, message="Internal error"))


def handle_jsonrpc_message_with_progress(message: str, progress_ctx: ProgressContext) -> str:
    """Handle JSON-RPC message with progress streaming support.

    Sets the progress context in thread-local storage so handlers can emit
    progress events via get_progress_context().

    Args:
        message: JSON-RPC request as string
        progress_ctx: Progress context for emitting events

    Returns:
        JSON-RPC response as string
    """
    set_progress_context(progress_ctx)
    try:
        return handle_jsonrpc_message(message)
    finally:
        try:
            progress_ctx.close()
        except Exception as e:
            logger.warning("Failed to close progress context: %s", e)
        set_progress_context(None)
