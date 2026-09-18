"""Cross-process lock for single-apply schema migrations.

Multiple processes (the TCP sidecar server, any background workers, per-request
spawns, a CLI) can open the same SQLite database near-simultaneously at app
launch. Without serialization they race through the migration chain, including
destructive table-recreation migrations. This module provides a file-based
advisory lock so only one process applies migrations at a time. The lock is
separate from SQLite's own transactions, so migrations that manage their own
``BEGIN IMMEDIATE`` are unaffected.
"""

from __future__ import annotations

import contextlib
from collections.abc import Iterator
from pathlib import Path

from filelock import FileLock

# Generous timeout: destructive migrations on a large database can take several
# seconds, and contending processes must wait for the applier to finish.
_LOCK_TIMEOUT_SECONDS = 60


@contextlib.contextmanager
def schema_migration_lock(db_path: Path | str | None) -> Iterator[None]:
    """Hold a cross-process lock while applying schema migrations.

    Args:
        db_path: Path to the SQLite database file. The lock file is derived as
            ``<db_path>.migrate.lock`` so each database gets its own lock. For
            in-memory databases or when no path is given, locking is skipped
            (each in-memory database is private to its process).
    """
    if db_path is None or str(db_path) == ":memory:":
        yield
        return

    lock_path = Path(f"{db_path}.migrate.lock")
    with FileLock(str(lock_path), timeout=_LOCK_TIMEOUT_SECONDS):
        yield
