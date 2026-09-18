//! Library root for the tauri-python-sidecar template.
//!
//! This file is the supervision layer: it starts the Python sidecar TCP server,
//! watches it, restarts it with capped backoff when it dies, cleans up stale
//! workers (and a stale SQLite WAL) left behind by a previous crash, and shuts
//! everything down without orphaning a process.
//!
//! Everything app-specific from the codebase this was extracted from — hotkeys,
//! a tray icon, clipboard capture, extra windows — has been removed. What is
//! left is the part that is hard to get right and is documented nowhere else.

pub mod commands;
pub mod paths;
pub mod sidecar_client;
pub mod state;
pub mod supervisor;

use state::{HarnessSupervisor, SidecarSupervisor};
use std::sync::atomic::Ordering;
use tauri::{Emitter, Manager};

/// A supervised background process, identified by the lock file it writes.
///
/// The Python side writes its PID to `<data dir>/.index/<lock_file>` and removes
/// the file on a clean exit, so a lock file that outlives its process is the
/// signal that the previous run crashed. Cleanup at startup and at shutdown both
/// walk this registry, which is why adding a second worker to the template is a
/// one-line change here rather than four edits scattered through the file.
struct Worker {
    /// File name under `.index/`, matching the name the Python process locks.
    lock_file: &'static str,
    /// Human-readable name used in log lines.
    name: &'static str,
}

/// The processes this app supervises. The template ships exactly one.
const WORKERS: &[Worker] = &[Worker {
    lock_file: "sidecar_server.lock",
    name: "Sidecar server",
}];

/// SQLite database file the sidecar owns, relative to the data directory.
/// Named here because startup WAL recovery has to find it before any Python runs.
const DB_FILENAME: &str = "sidecar.db";

/// Image name of the frozen sidecar binary, used for the by-name process sweep
/// before an update overwrites it.
const SIDECAR_IMAGE_NAME: &str = "py-sidecar.exe";

/// Restart backoff delays (seconds) for the sidecar server monitor.
///
/// The ladder is deliberately short and capped rather than exponential-forever:
/// a sidecar that dies is almost always either (a) transiently unlucky — a port
/// still in TIME_WAIT, a database still locked by the process that just died —
/// in which case one or two seconds is enough, or (b) broken in a way that more
/// waiting will not fix, in which case the user needs to be told rather than
/// left watching an app that retries silently for ten minutes. Five attempts
/// covers (a); after that we emit `failed` and stop.
const SIDECAR_RESTART_DELAYS_SECS: [u64; 5] = [1, 2, 5, 5, 5];

/// Window after which the sidecar restart attempt counter resets.
///
/// Without this, an app left open for a week would exhaust its five attempts on
/// five unrelated crashes months apart and then never restart again. Five
/// minutes of health is treated as "that incident is over".
const SIDECAR_RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// Spawn the persistent sidecar TCP server process, returning the child handle.
fn spawn_sidecar_server_process() -> Result<std::process::Child, String> {
    let config = paths::get_sidecar_config()?;
    let mut cmd = config.sidecar_server_command();

    // Add worker name for process identification
    cmd.arg("--worker-name=sidecar_server");

    // Tell the sidecar where the data directory is. The supervisor owns that
    // decision (see `paths::get_data_dir`), so it must inject it rather than let
    // the child guess from its working directory. In dev the two happen to
    // coincide; in an installed build they do not — the data directory is under
    // %APPDATA% while the process starts elsewhere — and a child that guesses
    // writes its session token where the Rust client will never look for it.
    // The symptom is a silent authentication failure, so it is injected in both
    // modes and not only where it is strictly required.
    cmd.env("SIDECAR_DATA_DIR", paths::get_data_dir());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start sidecar server: {}", e))?;

    log::info!(
        "Sidecar TCP server started (pid={}, dev_mode={})",
        child.id(),
        config.is_dev
    );
    Ok(child)
}

/// Start the persistent sidecar TCP server and supervise it.
///
/// Keeps the child handle in managed state (`SidecarSupervisor`) and spawns a
/// monitor task that restarts the server on unexpected exit with capped
/// exponential backoff. Intentional shutdown (app exit, pre-update cleanup)
/// sets `shutting_down` which suppresses restarts.
fn start_sidecar_server(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let child = spawn_sidecar_server_process()?;

    let supervisor = app.state::<SidecarSupervisor>();
    *supervisor.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    spawn_sidecar_monitor(app.clone());
    Ok(())
}

/// Monitor the sidecar server child process and restart it on unexpected exit.
///
/// Emits `sidecar-status` events to the frontend:
/// - `{ "status": "down", "attempt": n }` when an unexpected exit is detected
/// - `{ "status": "restarted", "attempt": n }` after a successful restart
/// - `{ "status": "failed", "attempt": n }` when giving up after repeated crashes
fn spawn_sidecar_monitor(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut attempts: usize = 0;
        let mut window_start = std::time::Instant::now();

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            let supervisor = app.state::<SidecarSupervisor>();
            if supervisor.shutting_down.load(Ordering::SeqCst) {
                log::debug!("Sidecar monitor: shutdown in progress, stopping");
                return;
            }

            // Poll the child without holding the lock across an await point
            let exit_status = {
                let mut guard = supervisor.child.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) => {
                        *guard = None;
                        Some(status)
                    }
                    Some(Ok(None)) => None, // still running
                    Some(Err(e)) => {
                        log::warn!("Sidecar monitor: failed to poll child: {}", e);
                        None
                    }
                    None => {
                        log::debug!("Sidecar monitor: no child to supervise, stopping");
                        return;
                    }
                }
            };

            let Some(status) = exit_status else { continue };

            // Re-check the flag: cleanup may have killed the child intentionally
            if supervisor.shutting_down.load(Ordering::SeqCst) {
                return;
            }

            log::error!(
                "Sidecar server exited unexpectedly (status: {:?}), attempting restart",
                status
            );

            // Reset the attempt counter if the last failure window has passed
            if window_start.elapsed() > SIDECAR_RESTART_WINDOW {
                attempts = 0;
                window_start = std::time::Instant::now();
            }

            let _ = app.emit(
                "sidecar-status",
                serde_json::json!({ "status": "down", "attempt": attempts + 1 }),
            );

            // Restart with capped backoff; retry until success or give-up
            loop {
                if attempts >= SIDECAR_RESTART_DELAYS_SECS.len() {
                    log::error!(
                        "Sidecar server crashed {} times within {:?}, giving up on restarts",
                        attempts,
                        SIDECAR_RESTART_WINDOW
                    );
                    let _ = app.emit(
                        "sidecar-status",
                        serde_json::json!({ "status": "failed", "attempt": attempts }),
                    );
                    return;
                }

                let delay = SIDECAR_RESTART_DELAYS_SECS[attempts];
                attempts += 1;
                log::warn!(
                    "Restarting sidecar server in {}s (attempt {}/{})",
                    delay,
                    attempts,
                    SIDECAR_RESTART_DELAYS_SECS.len()
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;

                if supervisor.shutting_down.load(Ordering::SeqCst) {
                    return;
                }

                match spawn_sidecar_server_process() {
                    Ok(child) => {
                        *supervisor.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                        log::info!("Sidecar server restarted (attempt {})", attempts);
                        let _ = app.emit(
                            "sidecar-status",
                            serde_json::json!({ "status": "restarted", "attempt": attempts }),
                        );
                        break;
                    }
                    Err(e) => {
                        log::error!("Sidecar server restart failed: {}", e);
                    }
                }
            }
        }
    });
}

/// Check if a process with given PID is currently running
fn is_process_running(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        // Windows: Use tasklist with CREATE_NO_WINDOW to prevent console popup
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .creation_flags(CREATE_NO_WINDOW) // Prevent console window
            .output()
        {
            Ok(output) => {
                if let Ok(stdout) = String::from_utf8(output.stdout) {
                    return stdout.contains(&pid.to_string());
                }
                false
            }
            Err(e) => {
                log::warn!("tasklist command failed for PID {}: {}", pid, e);
                // Conservative: assume alive if can't determine
                true
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Unix: Use ps command
        if let Ok(output) = std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
        {
            return output.status.success();
        }
        false
    }
}

/// Gracefully shutdown a worker process with timeout
///
/// Sends SIGTERM (or Windows equivalent), waits up to timeout_secs,
/// then force kills if still alive. This allows Python cleanup to run.
fn graceful_shutdown_worker(pid: u32, timeout_secs: u64) -> Result<(), String> {
    use std::thread;
    use std::time::Duration;

    log::info!(
        "Attempting graceful shutdown of PID {} (timeout: {}s)",
        pid,
        timeout_secs
    );

    // Step 1: Send graceful termination signal
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        // Windows: taskkill without /F flag sends WM_CLOSE (graceful)
        let result = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW) // Prevent console window
            .output();

        if let Err(e) = result {
            log::warn!(
                "Failed to send graceful shutdown signal to PID {}: {}",
                pid,
                e
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Unix: Send SIGTERM (graceful)
        let result = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output();

        if let Err(e) = result {
            log::warn!("Failed to send SIGTERM to PID {}: {}", pid, e);
        }
    }

    // Step 2: Poll for process termination (check every 200ms)
    let poll_interval = Duration::from_millis(200);
    let max_polls = (timeout_secs * 1000) / 200;

    for poll_count in 0..max_polls {
        thread::sleep(poll_interval);

        if !is_process_running(pid) {
            log::info!(
                "Process {} terminated gracefully after {:.1}s",
                pid,
                (poll_count * 200) as f64 / 1000.0
            );
            return Ok(());
        }
    }

    // Step 3: Force kill if still alive after timeout
    log::warn!("Process {} did not terminate gracefully, force killing", pid);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .creation_flags(CREATE_NO_WINDOW) // Prevent console window
            .output()
            .map_err(|e| format!("Failed to force kill PID {}: {}", pid, e))?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .output()
            .map_err(|e| format!("Failed to force kill PID {}: {}", pid, e))?;
    }

    log::info!("Process {} force killed", pid);
    Ok(())
}

/// Read a worker's PID from its lock file.
///
/// Returns `Ok(None)` when the lock file is absent or vanished mid-read — both
/// mean there is nothing left to clean up. The vanished case is a real TOCTOU
/// race: the worker's own cleanup can delete the file between the `exists()`
/// check and the read.
fn read_lock_pid(lock_file: &std::path::Path, worker_name: &str) -> Result<Option<u32>, String> {
    if !lock_file.exists() {
        log::debug!("{} lock file not found, skipping", worker_name);
        return Ok(None);
    }

    let pid_str = match std::fs::read_to_string(lock_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!(
                "{} lock file disappeared during read, already cleaned",
                worker_name
            );
            return Ok(None);
        }
        Err(e) => return Err(format!("Failed to read {} lock file: {}", worker_name, e)),
    };

    pid_str
        .trim()
        .parse::<u32>()
        .map(Some)
        .map_err(|e| format!("Invalid PID in {} lock file: {}", worker_name, e))
}

/// Remove a worker's lock file after its process is gone.
///
/// A missing file is success, not an error: the worker's own cleanup usually
/// wins this race, and that is the good outcome.
fn remove_lock_file(lock_file: &std::path::Path, worker_name: &str) -> Result<(), String> {
    match std::fs::remove_file(lock_file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!("{} lock file already removed by worker", worker_name);
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to remove {} lock file: {}", worker_name, e);
            Err(format!("Failed to remove lock file: {}", e))
        }
    }
}

/// Cleanup a single stale worker by force killing immediately (for startup cleanup)
fn cleanup_worker_lock(lock_file: &std::path::Path, worker_name: &str) -> Result<(), String> {
    let Some(pid) = read_lock_pid(lock_file, worker_name)? else {
        return Ok(());
    };

    // Check if process is actually running
    if !is_process_running(pid) {
        log::info!(
            "{} process (PID {}) not running, removing stale lock",
            worker_name,
            pid
        );
        if let Err(e) = std::fs::remove_file(lock_file) {
            log::error!("Failed to remove stale {} lock file: {}", worker_name, e);
        }
        return Ok(());
    }

    // Force kill stale worker immediately (skip graceful shutdown)
    // Zombie workers from crashes don't respond to signals, so graceful shutdown
    // would waste time. Force kill is safe and fast for cleanup at startup.
    log::info!("Force killing stale {} (PID {})", worker_name, pid);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        let result = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        if let Err(e) = result {
            log::warn!("Failed to force kill {}: {}", worker_name, e);
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let result = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .output();

        if let Err(e) = result {
            log::warn!("Failed to force kill {}: {}", worker_name, e);
        }
    }

    // Poll for process death (max 200ms) instead of blind sleep
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        if !is_process_running(pid) {
            break;
        }
    }

    if is_process_running(pid) {
        log::warn!(
            "{} still alive after force kill, continuing anyway",
            worker_name
        );
    }

    remove_lock_file(lock_file, worker_name)?;

    log::info!("{} shutdown complete", worker_name);
    Ok(())
}

/// Cleanup a single worker with graceful shutdown timeout (for normal app exit)
fn cleanup_worker_lock_with_timeout(
    lock_file: &std::path::Path,
    worker_name: &str,
    timeout_secs: u64,
) -> Result<(), String> {
    let Some(pid) = read_lock_pid(lock_file, worker_name)? else {
        return Ok(());
    };

    // Check if process is actually running
    if !is_process_running(pid) {
        log::info!(
            "{} process (PID {}) not running, removing stale lock",
            worker_name,
            pid
        );
        if let Err(e) = std::fs::remove_file(lock_file) {
            log::error!("Failed to remove stale {} lock file: {}", worker_name, e);
        }
        return Ok(());
    }

    // Gracefully shutdown the worker with timeout (for normal app exit)
    log::info!("Shutting down {} (PID {})", worker_name, pid);
    graceful_shutdown_worker(pid, timeout_secs)?;

    remove_lock_file(lock_file, worker_name)?;

    log::info!("{} shutdown complete", worker_name);
    Ok(())
}

/// Recover the SQLite database after a crash left write-ahead-log files behind.
///
/// SQLite WAL mode creates `.db-wal` and `.db-shm` files that can be stale after
/// a force quit. Opening a connection triggers automatic WAL recovery and
/// checkpoint, so this runs before any worker touches the database.
fn recover_sqlite_wal(db_path: &std::path::Path) {
    if !db_path.exists() {
        return;
    }

    let wal_path = format!("{}-wal", db_path.display());
    let shm_path = format!("{}-shm", db_path.display());

    if !std::path::Path::new(&wal_path).exists() && !std::path::Path::new(&shm_path).exists() {
        log::debug!("No stale WAL files found, database is clean");
        return;
    }

    log::warn!("Found stale WAL files after crash, triggering checkpoint and recovery...");

    let conn = match rusqlite::Connection::open(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            log::error!("Failed to open database for WAL recovery: {}", e);
            log::error!(
                "Database may be locked or corrupted. \
                If errors persist, restore from backup: {}.backup",
                db_path.display()
            );
            // Non-fatal: workers will retry with busy_timeout
            return;
        }
    };

    // Force FULL checkpoint (merges WAL into main DB, truncates WAL)
    match conn.pragma_update(None, "wal_checkpoint", "RESTART") {
        Ok(_) => {
            log::info!("WAL checkpoint completed successfully");

            // Verify database is usable after recovery (health check).
            // A checkpoint that returns Ok only proves the WAL was replayed, not
            // that the resulting database is readable: a torn page or a truncated
            // main file still checkpoints "successfully" and then fails on the
            // first real query, inside a worker, minutes later. `SELECT 1` forces
            // SQLite to open and parse the schema here, where we can say so.
            match conn.query_row("SELECT 1", [], |_| Ok(())) {
                Ok(_) => log::info!("Database health check passed after WAL recovery"),
                Err(e) => {
                    log::error!(
                        "DATABASE CORRUPTION DETECTED: Database unusable after WAL recovery: {}",
                        e
                    );
                    log::error!(
                        "Action required: Database may be corrupted. \
                        Check logs and consider restoring from backup at: {}.backup",
                        db_path.display()
                    );
                    // Continue startup - workers will encounter the error and log it
                    // User will see no data and can investigate via logs
                }
            }
        }
        Err(e) => {
            log::warn!("WAL checkpoint failed (non-fatal): {}", e);
            log::warn!(
                "Database may be in inconsistent state. \
                If errors persist, restore from backup: {}.backup",
                db_path.display()
            );
        }
    }

    drop(conn); // Close connection explicitly
}

/// Force-kill any harness process left behind by a crash.
///
/// Harness locks are dynamic (`harness_<id>.lock`), so glob rather than
/// enumerate. A crash or force-kill leaves the lock plus a zombie holding
/// the port; reuse the force-kill-by-PID path so a stale harness can't be
/// mistaken for a live external daemon on next start.
fn cleanup_stale_harness_locks(index_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(index_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_harness_lock = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("harness_") && n.ends_with(".lock"))
            .unwrap_or(false);
        if is_harness_lock {
            log::warn!("Found stale harness lock at startup: {}", path.display());
            if let Err(e) = cleanup_worker_lock(&path, "Stale harness") {
                log::error!("Failed to cleanup stale harness lock: {}", e);
            }
        }
    }
}

/// Cleanup stale workers and lock files at app startup
///
/// This runs BEFORE spawning new workers to ensure clean slate.
/// Handles three cases:
/// 1. No lock file: Fresh start (no action needed)
/// 2. Lock file + dead process: Stale lock from crash (remove lock file)
/// 3. Lock file + live process: Zombie worker from previous run (gracefully kill + remove lock)
fn cleanup_stale_workers() -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Checking for stale workers at startup...");

    let index_dir = paths::get_index_dir();

    // Ensure .index directory exists
    std::fs::create_dir_all(&index_dir)?;

    // Clear any stale shutdown flag from crash/force-quit
    let shutdown_flag = index_dir.join("shutdown.flag");
    if shutdown_flag.exists() {
        log::debug!("Removing stale shutdown flag from previous run");
        let _ = std::fs::remove_file(&shutdown_flag);
    }

    recover_sqlite_wal(&paths::get_data_dir().join(DB_FILENAME));

    // Parallel cleanup of stale workers (200ms total vs 800ms sequential)
    let stale_workers: Vec<&Worker> = WORKERS
        .iter()
        .filter(|w| index_dir.join(w.lock_file).exists())
        .collect();

    if stale_workers.is_empty() {
        log::info!("No stale workers found, proceeding with clean startup");
    } else {
        log::info!(
            "Found {} stale worker(s), cleaning up in parallel...",
            stale_workers.len()
        );

        // Spawn parallel cleanup threads
        let handles: Vec<_> = stale_workers
            .into_iter()
            .map(|worker| {
                let lock_path = index_dir.join(worker.lock_file);
                let name = format!("Stale {}", worker.name);
                std::thread::spawn(move || {
                    log::warn!("Found existing {} lock file at startup", name);
                    if let Err(e) = cleanup_worker_lock(&lock_path, &name) {
                        log::error!("Failed to cleanup {}: {}", name, e);
                    }
                })
            })
            .collect();

        // Wait for all cleanup threads to complete (max 200ms since force kill is fast)
        for handle in handles {
            if let Err(e) = handle.join() {
                log::error!("Worker cleanup thread panicked: {:?}", e);
            }
        }

        log::info!("Stale worker cleanup complete");
    }

    cleanup_stale_harness_locks(&index_dir);

    Ok(())
}

/// Kill background worker processes on app shutdown (parallel, 3s timeout)
///
/// Uses shutdown flag file for cross-platform worker notification (Windows + Unix).
/// Workers poll .index/shutdown.flag and exit gracefully when detected — Windows
/// has no SIGTERM a Python process can usefully catch, so the flag file is the
/// portable signal.
fn cleanup_workers() {
    use std::fs;
    use std::thread;

    log::info!("Cleaning up background workers (parallel)...");

    let index_dir = paths::get_index_dir();

    // Step 1: Write shutdown flag file (instant signal to all workers)
    // Workers poll this file and exit gracefully when detected
    let shutdown_flag = index_dir.join("shutdown.flag");
    if let Err(e) = fs::write(&shutdown_flag, "shutdown") {
        log::warn!("Failed to write shutdown flag: {}", e);
    } else {
        log::debug!("Shutdown flag written to {}", shutdown_flag.display());
    }

    // Step 2: Spawn parallel cleanup threads (3s timeout each, parallel = 3s total max)
    let handles: Vec<_> = WORKERS
        .iter()
        .map(|worker| {
            let lock_path = index_dir.join(worker.lock_file);
            let name = worker.name;
            thread::spawn(move || {
                if let Err(e) = cleanup_worker_lock_with_timeout(&lock_path, name, 3) {
                    log::error!("{} cleanup failed: {}", name, e);
                }
            })
        })
        .collect();

    // Step 3: Wait for all threads (max 3s since they run in parallel)
    for handle in handles {
        if let Err(e) = handle.join() {
            log::error!("Worker cleanup thread panicked: {:?}", e);
        }
    }

    // Step 4: Remove shutdown flag file
    if let Err(e) = fs::remove_file(&shutdown_flag) {
        // Not an error if file doesn't exist (worker may have cleaned it)
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("Failed to remove shutdown flag: {}", e);
        }
    }

    log::info!("Worker cleanup complete");
}

/// Check if any sidecar process is currently running (by image name).
///
/// Returns `false` on error (unlike `is_process_running` which assumes alive).
/// For the update flow, false-negative is safe -- the installer will retry or overwrite.
fn is_sidecar_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {}", SIDECAR_IMAGE_NAME), "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) => {
                if let Ok(stdout) = String::from_utf8(output.stdout) {
                    return stdout.contains(SIDECAR_IMAGE_NAME);
                }
                false
            }
            Err(_) => false,
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(output) = std::process::Command::new("pgrep")
            .args(["-f", SIDECAR_IMAGE_NAME.trim_end_matches(".exe")])
            .output()
        {
            return output.status.success();
        }
        false
    }
}

/// Kill all sidecar processes before update installation.
///
/// Called from the frontend before `update.install()` to ensure the NSIS
/// installer can overwrite the sidecar binary without file-lock errors.
#[tauri::command]
async fn prepare_for_update(app: tauri::AppHandle) -> Result<(), String> {
    log::info!("Preparing for update: killing all sidecar processes");

    // Intentional shutdown: prevent the sidecar monitor from restarting
    app.state::<SidecarSupervisor>()
        .shutting_down
        .store(true, Ordering::SeqCst);

    cleanup_workers();

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", SIDECAR_IMAGE_NAME, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("pkill")
            .args(["-f", SIDECAR_IMAGE_NAME.trim_end_matches(".exe")])
            .output();
    }

    let poll_interval_ms: u64 = 200;
    let max_polls: u64 = 10;

    for poll in 0..max_polls {
        if !is_sidecar_running() {
            let elapsed = poll as f64 * poll_interval_ms as f64 / 1000.0;
            log::info!("All sidecar processes confirmed dead after {:.1}s", elapsed);
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;
    }

    let total_wait = max_polls * poll_interval_ms / 1000;
    log::warn!(
        "Sidecar may still be running after {}s, proceeding with update anyway",
        total_wait
    );
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(SidecarSupervisor::default())
        .manage(HarnessSupervisor::default())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            // Cleanup any stale workers from previous runs (BLOCKING, SYNCHRONOUS).
            // This prevents race conditions where a freshly spawned worker meets
            // the previous run's lock file and refuses to start.
            log::info!("Cleaning up stale resources from previous runs (blocking)...");
            if let Err(e) = cleanup_stale_workers() {
                log::error!(
                    "Stale worker cleanup failed, continuing with degraded state: {}",
                    e
                );
                // Don't panic - allow app to start even if cleanup fails
                // Workers will detect conflicts during initialization and handle them
            }
            log::info!("Stale resource cleanup complete, spawning workers...");

            // Start the persistent sidecar TCP server (supervised, auto-restarts
            // on crash). A failure here — a missing sidecar binary in a corrupted
            // bundle — is logged and the app continues in degraded mode instead of
            // aborting startup: the window still opens and requests fail with a
            // readable error, which is far easier to report than a silent no-launch.
            if let Err(e) = start_sidecar_server(app.handle()) {
                log::error!("Failed to start sidecar server: {}", e);
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::sidecar::sidecar_request,
            prepare_for_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                // Runs on every normal exit path (window close, app.exit) so
                // background workers are never orphaned. OS force-kill is not
                // catchable — startup stale-worker cleanup covers that case.
                let supervisor = app_handle.state::<SidecarSupervisor>();
                supervisor.shutting_down.store(true, Ordering::SeqCst);
                // Drop the supervised child handle; cleanup_workers kills the
                // process via its lock-file PID.
                supervisor
                    .child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                // Stop all supervised harness children so none are orphaned.
                app_handle.state::<HarnessSupervisor>().stop_all();
                cleanup_workers();
            }
        });
}
