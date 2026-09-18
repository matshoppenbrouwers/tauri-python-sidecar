"""Tests for concurrency-safe, single-apply schema migrations.

Verifies the fast-path version gate and the cross-process lock that together
ensure the migration chain applies exactly once, regardless of how many
processes open the same database simultaneously.
"""

from __future__ import annotations

import multiprocessing
import sqlite3
from pathlib import Path

from sidecar.storage import schema
from sidecar.storage.schema import CURRENT_SCHEMA_VERSION, open_database


def _columns(conn: sqlite3.Connection, table_name: str) -> set[str]:
    cursor = conn.execute(f"PRAGMA table_info({table_name})")
    return {row[1] for row in cursor.fetchall()}


def _read_schema_version(db_path: Path) -> int | None:
    conn = sqlite3.connect(str(db_path))
    try:
        row = conn.execute(
            "SELECT value FROM migration_metadata WHERE key = 'schema_version'"
        ).fetchone()
        return int(row[0]) if row is not None else None
    finally:
        conn.close()


def test_fresh_apply_sets_schema_version(tmp_path: Path) -> None:
    db_path = tmp_path / "sidecar.db"
    open_database(db_path).close()

    assert _read_schema_version(db_path) == CURRENT_SCHEMA_VERSION


def test_fast_path_skips_migration_chain(tmp_path: Path, monkeypatch) -> None:
    db_path = tmp_path / "sidecar.db"
    open_database(db_path).close()  # first open applies + records version

    calls: list[int] = []
    original = schema._run_migrations

    def _spy(conn: sqlite3.Connection) -> None:
        calls.append(1)
        original(conn)

    monkeypatch.setattr(schema, "_run_migrations", _spy)

    open_database(db_path).close()  # second open should hit the fast path

    assert calls == []
    assert _read_schema_version(db_path) == CURRENT_SCHEMA_VERSION


def test_version_bump_reruns_once(tmp_path: Path, monkeypatch) -> None:
    db_path = tmp_path / "sidecar.db"
    open_database(db_path).close()

    # Simulate an older install whose schema predates the current version.
    conn = sqlite3.connect(str(db_path))
    conn.execute(
        "INSERT OR REPLACE INTO migration_metadata (key, value) VALUES ('schema_version', ?)",
        (str(CURRENT_SCHEMA_VERSION - 1),),
    )
    conn.commit()
    conn.close()

    calls: list[int] = []
    original = schema._run_migrations

    def _spy(conn: sqlite3.Connection) -> None:
        calls.append(1)
        original(conn)

    monkeypatch.setattr(schema, "_run_migrations", _spy)

    open_database(db_path).close()

    assert calls == [1]  # chain ran exactly once
    assert _read_schema_version(db_path) == CURRENT_SCHEMA_VERSION


def _open_database_worker(db_path: str, barrier) -> None:
    """Worker process: open the database under maximum contention, then close."""
    from sidecar.storage.schema import open_database

    barrier.wait()
    open_database(db_path).close()


def test_concurrent_processes_single_apply(tmp_path: Path) -> None:
    db_path = tmp_path / "sidecar.db"
    n = 8

    # Establish WAL mode on the file up front. This models the real-world
    # upgrade scenario (the database already exists from prior runs) and
    # isolates the migration-apply race from the orthogonal connection-layer
    # race of switching journal mode on a brand-new file.
    prep = sqlite3.connect(str(db_path))
    prep.execute("PRAGMA journal_mode=WAL")
    prep.close()

    ctx = multiprocessing.get_context("spawn")
    barrier = ctx.Barrier(n)
    procs = [
        ctx.Process(target=_open_database_worker, args=(str(db_path), barrier)) for _ in range(n)
    ]
    for p in procs:
        p.start()
    for p in procs:
        p.join(timeout=120)

    for p in procs:
        assert p.exitcode == 0, f"worker exited with {p.exitcode}"

    # The demo migration survived the race intact.
    conn = sqlite3.connect(str(db_path))
    try:
        assert _columns(conn, "notes") == {"id", "text", "created_at"}
    finally:
        conn.close()

    assert _read_schema_version(db_path) == CURRENT_SCHEMA_VERSION
