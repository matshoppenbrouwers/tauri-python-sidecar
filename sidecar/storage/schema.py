"""SQLite schema management module.

Handles database schema initialization, migrations, and index creation.

The app this was extracted from had 27 migrations behind ``_run_migrations``;
the template keeps one, because the part worth copying is the *runner*, not the
migrations. Read ``init_schema`` — the sentinel fast path, the cross-process
lock and the double-check inside it are what make the chain apply exactly once
when several processes open the same database at app launch.
"""

from __future__ import annotations

import logging
import sqlite3
from pathlib import Path
from typing import TYPE_CHECKING

from .migration_lock import schema_migration_lock

if TYPE_CHECKING:  # pragma: no cover - typing only
    from sqlite3 import Connection

logger = logging.getLogger(__name__)

# Bump whenever a new _migrate_0XX is added. This sentinel invalidates the
# fast path and forces the migration chain to run exactly once on upgrade.
CURRENT_SCHEMA_VERSION = 1


def init_schema(conn: Connection, db_path: Path | str | None = None) -> None:
    """Initialize database schema with all required tables and indexes.

    Migrations apply exactly once across processes: a cheap ``schema_version``
    sentinel short-circuits the common case, and a cross-process file lock
    serializes the apply when the schema is behind. This prevents the migration
    race where several processes (sidecar server, background workers,
    per-request spawns) open the same database at launch and concurrently run
    destructive table-recreation migrations.

    Args:
        conn: SQLite database connection
        db_path: Path to the database file, used to derive the migration lock.
            None (in-memory or direct test connections) skips locking.
    """
    _create_migration_metadata_table(conn)

    # Fast path: schema already current -> no lock, no table churn.
    if _get_schema_version(conn) == CURRENT_SCHEMA_VERSION:
        return

    with schema_migration_lock(db_path):
        # Double-checked: another process may have applied while we waited.
        if _get_schema_version(conn) == CURRENT_SCHEMA_VERSION:
            return
        _apply_schema(conn)
        _set_schema_version(conn, CURRENT_SCHEMA_VERSION)


def _apply_schema(conn: Connection) -> None:
    """Create base tables/indexes and run the migration chain (single applier)."""
    with conn:
        _run_migrations(conn)


def _get_schema_version(conn: Connection) -> int | None:
    """Return the recorded schema version, or None if absent/unparseable."""
    cursor = conn.execute("SELECT value FROM migration_metadata WHERE key = 'schema_version'")
    row = cursor.fetchone()
    if row is None:
        return None
    try:
        return int(row[0])
    except (TypeError, ValueError):
        return None


def _set_schema_version(conn: Connection, version: int) -> None:
    """Record the schema version sentinel after a successful apply."""
    conn.execute(
        "INSERT OR REPLACE INTO migration_metadata (key, value) VALUES ('schema_version', ?)",
        (str(version),),
    )
    conn.commit()


def _create_migration_metadata_table(conn: Connection) -> None:
    """Create migration metadata table."""
    conn.execute("""
        CREATE TABLE IF NOT EXISTS migration_metadata (
            key TEXT PRIMARY KEY,
            value TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
    """)


def _run_migrations(conn: Connection) -> None:
    """Run the migration chain in order.

    Append new ``_migrate_0XX`` functions here and bump
    ``CURRENT_SCHEMA_VERSION``. Each migration must be idempotent on its own
    (``IF NOT EXISTS``, guarded ``ALTER TABLE``) so that a crash part-way
    through the chain leaves a database the next run can finish.
    """
    _migrate_001_notes(conn)


def _migrate_001_notes(conn: Connection) -> None:
    """Demo migration: the single table this template's handlers use."""
    conn.execute("""
        CREATE TABLE IF NOT EXISTS notes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            text TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
    """)


def open_database(db_path: Path | str) -> sqlite3.Connection:
    """Open the sidecar database, applying migrations if needed.

    WAL is what makes a multi-process desktop app viable at all: readers do not
    block the writer. It is also why the Rust side needs the checkpoint-recovery
    path documented in ``docs/wal-recovery.md`` — a process killed mid-write
    leaves a ``-wal`` file that someone has to fold back in.
    """
    Path(db_path).parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA journal_mode=WAL")
    init_schema(conn, db_path)
    return conn
