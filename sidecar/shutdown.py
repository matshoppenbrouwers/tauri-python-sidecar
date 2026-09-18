"""Cross-platform shutdown detection via flag file.

Workers poll should_shutdown() to detect app exit.
File-based approach works reliably on Windows where signals don't.

References:
- https://stackoverflow.com/questions/47306805/signal-sigterm-not-received-by-subprocess-on-windows
- https://stefan.sofa-rockers.org/2013/08/15/handling-sub-process-hierarchies-python-linux-os-x/
"""

from __future__ import annotations

import logging
from pathlib import Path

logger = logging.getLogger(__name__)

INDEX_DIR = Path(".index")
SHUTDOWN_FLAG = INDEX_DIR / "shutdown.flag"

_shutdown_detected = False  # Cache to avoid repeated stat() calls


def should_shutdown() -> bool:
    """Check if shutdown has been requested.

    Returns cached result after first detection (process should exit soon anyway).
    File existence check is atomic on NTFS/ext4.
    """
    global _shutdown_detected
    if _shutdown_detected:
        return True
    if SHUTDOWN_FLAG.exists():
        _shutdown_detected = True
        logger.info("Shutdown flag detected")
        return True
    return False


def clear_shutdown_flag() -> None:
    """Remove shutdown flag (called at startup and after cleanup)."""
    global _shutdown_detected
    _shutdown_detected = False
    SHUTDOWN_FLAG.unlink(missing_ok=True)
