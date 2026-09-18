# Startup WAL recovery, and why the `SELECT 1`

A supervised sidecar is a process that gets killed. By the supervisor during an
update, by the user through Task Manager, by a crash, by a machine losing power.
If it was mid-write to SQLite when that happened, it leaves `sidecar.db-wal` and
`sidecar.db-shm` behind.

The Rust side checkpoints them at startup, before any Python runs:
`recover_sqlite_wal()` in `src-tauri/src/lib.rs`.

## Why Rust does it and not Python

Whichever process opens the database first triggers SQLite's own WAL recovery
implicitly. Doing it deliberately, from the supervisor, before spawning
anything, means the recovery happens in one known place where its outcome can be
logged and acted on — rather than inside whichever of several workers happened
to win the race, where a failure surfaces minutes later as an unrelated query
error.

`rusqlite` is compiled with the `bundled` feature so the checkpoint does not
depend on whatever SQLite version the host happens to have.

## The sequence

1. If the database file does not exist, return. Nothing to recover.
2. If neither `-wal` nor `-shm` exists, log at debug and return. This is the
   normal case and it costs one `stat`.
3. Otherwise log a warning — the app is about to recover from a crash, and that
   is worth a line in the log the user will send you.
4. Open the connection. A failure here is **non-fatal**: workers open with
   `busy_timeout` and will retry. The error message names the backup path
   instead of throwing.
5. `PRAGMA wal_checkpoint(RESTART)` — merge the WAL into the main database and
   truncate it.
6. **`SELECT 1`.**

## Why step 6 exists

A successful checkpoint proves that the write-ahead log was replayed. It does
not prove that the resulting database is readable.

`wal_checkpoint` returns `Ok` when the pages it was asked to move were moved. A
torn page in the main database file, a truncated file from a disk that filled
mid-write, or corruption in a region the checkpoint never touched all survive
that call untouched. The database then fails on the first real query — inside a
worker, on a background thread, minutes after launch, with a stack trace that
points at whatever innocent code issued that query.

`SELECT 1` forces SQLite to open and parse the schema right here, in the one
place that is explicitly about recovery. When it fails, the log says
`DATABASE CORRUPTION DETECTED` next to the checkpoint that preceded it, and the
message names the action and the backup path.

The check is deliberately cheap and deliberately not `PRAGMA integrity_check`. A
full integrity check reads every page and can take minutes on a large database,
at application startup, on a path that runs after every ungraceful exit. The
tradeoff is stated plainly: `SELECT 1` catches the failures that make the
database unusable, not every form of corruption.

## What it does not do

Recovery does not roll back. If the crash lost a partial transaction, SQLite
discards it — that is the WAL working as designed. And the code continues
startup even after a failed health check, rather than refusing to launch: an app
that starts and reports a broken database is more useful than one that will not
start and says nothing.

The `.backup` path named in the log messages is a convention this template
mentions but does not implement. If your app takes backups (the updater flow in
`ui/src/hooks/useUpdater.ts` is the natural place), point those messages at
where they actually land.
