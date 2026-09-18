"""
JSON-RPC error handling with security hardening.

This module provides sanitized error handling for the JSON-RPC sidecar API.
All errors are logged server-side with full details, but only sanitized
messages are returned to the client to prevent information leakage.

The shape to copy is ``JsonRpcError.from_exception``: a chain of ``isinstance``
branches that decides, per exception type, how much is safe to tell the
renderer. Add your own application exception types to that chain — the rule is
that anything not explicitly whitelisted falls through to the generic
"An unexpected error occurred" at the bottom.
"""

import logging
import sqlite3
from dataclasses import dataclass
from typing import Any

logger = logging.getLogger(__name__)


class ErrorCode:
    """JSON-RPC 2.0 error codes (standard + application-specific)."""

    # JSON-RPC Standard
    PARSE_ERROR = -32700
    INVALID_REQUEST = -32600
    METHOD_NOT_FOUND = -32601
    INVALID_PARAMS = -32602
    INTERNAL_ERROR = -32603

    # Application-Specific. JSON-RPC reserves -32000 to -32099 for the server
    # to define; give each failure mode the renderer must react to differently
    # its own code, rather than overloading INTERNAL_ERROR.
    DATABASE_WRITE_FAILURE = -32002
    CONFIGURATION_ERROR = -32007


@dataclass(frozen=True)
class JsonRpcError(Exception):
    """
    JSON-RPC error with structured data.

    This exception type is used throughout the sidecar to represent errors
    that should be returned to the client via JSON-RPC error response.
    """

    code: int
    message: str
    data: dict[str, Any] | None = None

    def to_dict(self) -> dict[str, Any]:
        """Convert to JSON-RPC error object."""
        result = {"code": self.code, "message": self.message}
        if self.data:
            result["data"] = self.data
        return result

    @classmethod
    def from_exception(cls, exc: Exception, method: str) -> "JsonRpcError":
        """
        Convert Python exception to JSON-RPC error with sanitization.

        SECURITY: Logs full details server-side, returns sanitized message to user.

        Args:
            exc: The exception to convert
            method: The JSON-RPC method that raised the exception

        Returns:
            JsonRpcError with sanitized message
        """
        # Validation errors - safe to expose the message, it describes the
        # caller's own input and nothing about the server.
        if isinstance(exc, ValueError):
            return cls(
                code=ErrorCode.INVALID_PARAMS,
                message=str(exc),
                data={"field": getattr(exc, "field", None)},
            )

        if isinstance(exc, sqlite3.IntegrityError):
            logger.error("SQLite integrity error in %s: %s", method, exc, exc_info=True)
            return cls(
                code=ErrorCode.DATABASE_WRITE_FAILURE,
                message="Failed to update related records",
                data=None,
            )

        # Storage errors - sanitize (don't expose database paths)
        if isinstance(exc, sqlite3.Error):
            logger.error("Storage error in %s: %s", method, exc, exc_info=True)
            return cls(
                code=ErrorCode.DATABASE_WRITE_FAILURE,
                message="Failed to access database",
                data=None,  # Don't leak internal details
            )

        # Unexpected errors - log full details, return generic message
        logger.critical(
            "Unexpected error in %s: %s",
            method,
            exc,
            exc_info=True,
            extra={"method": method, "error_type": type(exc).__name__},
        )
        return cls(
            code=ErrorCode.INTERNAL_ERROR,
            message="An unexpected error occurred",
            data=None,  # NEVER expose exception details to user
        )


def create_parse_error() -> JsonRpcError:
    """Create a parse error (invalid JSON)."""
    return JsonRpcError(code=ErrorCode.PARSE_ERROR, message="Invalid JSON in request")


def create_invalid_request_error(reason: str) -> JsonRpcError:
    """Create an invalid request error (missing required fields)."""
    return JsonRpcError(
        code=ErrorCode.INVALID_REQUEST,
        message="Invalid JSON-RPC request",
        data={"reason": reason},
    )


def create_method_not_found_error(method: str, available_methods: list[str]) -> JsonRpcError:
    """Create a method not found error."""
    return JsonRpcError(
        code=ErrorCode.METHOD_NOT_FOUND,
        message=f"Method not found: {method}",
        data={"available_methods": available_methods},
    )
