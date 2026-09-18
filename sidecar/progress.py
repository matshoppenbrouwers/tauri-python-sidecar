"""Thread-safe progress event streaming for TCP sidecar.

Provides a bridge between thread pool handlers and the async TCP writer,
allowing progress events to be streamed to the client during query execution.
"""

from __future__ import annotations

import json
import queue
import threading
from dataclasses import dataclass, field


@dataclass
class ProgressContext:
    """Thread-safe progress event emitter for TCP streaming.

    Used by handlers running in thread pool to emit progress events
    that get streamed to the TCP client by the async server loop.
    """

    _queue: queue.Queue = field(repr=False)

    def emit(self, step: str) -> None:
        """Emit a progress event (thread-safe).

        Args:
            step: User-friendly description of current step
        """
        if not isinstance(step, str):
            step = str(step)
        # Truncate excessively long messages
        if len(step) > 500:
            step = step[:497] + "..."

        event = {"type": "progress", "step": step}
        self._queue.put(json.dumps(event))

    def emit_event(self, event: dict) -> None:
        """Stream a structured event dict to the client (thread-safe).

        Unlike ``emit`` (human-readable progress steps), this forwards a full
        event — used to push structured domain events through to the renderer.
        """
        self._queue.put(json.dumps(event))

    def close(self) -> None:
        """Signal no more progress events (sentinel value)."""
        self._queue.put(None)


def create_progress_context() -> tuple[ProgressContext, queue.Queue]:
    """Create a new progress context and its underlying queue.

    Returns:
        Tuple of (ProgressContext for handler, queue for async streaming)
    """
    q: queue.Queue = queue.Queue()
    return ProgressContext(q), q


# Thread-local storage for progress context
_progress_context: threading.local = threading.local()


def set_progress_context(ctx: ProgressContext | None) -> None:
    """Set the progress context for the current thread."""
    _progress_context.ctx = ctx


def get_progress_context() -> ProgressContext | None:
    """Get the current progress context (thread-safe).

    Returns:
        ProgressContext if set, None otherwise
    """
    return getattr(_progress_context, "ctx", None)
