//! Per-harness process lifecycle: spawn, dynamic ports, health, restart.
//!
//! A *harness* here is any external process the app owns the lifecycle of — a
//! second Python service, a language server, a vendored CLI daemon. This module
//! generalizes the single-child `SidecarSupervisor` + `spawn_sidecar_monitor`
//! pattern (`state.rs`, `lib.rs`) into a map keyed by harness id, so one app can
//! supervise N of them. The template itself supervises only the Python sidecar;
//! this file is here because it is the shape the general case takes, and because
//! it is the direct answer to tauri-apps/plugins-workspace#3062.
//!
//! Launch specs are declarative: a `command`/`args`/`cwd`/`env` plus how to
//! inject the port and how to health-check. The supervisor spawns FOREGROUND
//! processes only. If the process you are supervising offers an "install as a
//! service/daemon" mode, do not use it: the OS service manager would then own
//! the process and fight this supervisor for restart rights.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::paths;
use crate::state::HarnessSupervisor;

/// Restart backoff delays (seconds), mirroring the sidecar monitor.
const HARNESS_RESTART_DELAYS_SECS: [u64; 5] = [1, 2, 5, 5, 5];
/// Window after which the restart attempt counter resets.
const HARNESS_RESTART_WINDOW: Duration = Duration::from_secs(300);
/// Timeout for a single health probe connection.
const HEALTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTH_IO_TIMEOUT: Duration = Duration::from_secs(3);

/// Declarative launch spec for a harness: how to start it, how it takes its
/// port, and how to tell that it is actually serving.
///
/// It derives `Deserialize` so the specs can come from a JSON catalogue rather
/// than being compiled in. Keep this struct the single place that defines that
/// wire shape.
#[derive(Clone, Debug, Deserialize)]
pub struct LaunchSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub port: PortSpec,
    pub health: HealthSpec,
}

/// How the harness's listen port is chosen and injected into the launch.
#[derive(Clone, Debug, Deserialize)]
pub struct PortSpec {
    /// `"fixed"` (use `value`, detect external instances) or `"dynamic"`
    /// (allocate a free port and inject it).
    pub strategy: String,
    /// Default/conventional port. For `fixed` this is the port; for `dynamic`
    /// it is the well-known port a user-installed daemon would occupy.
    #[serde(default)]
    pub value: Option<u16>,
    /// Env var to inject the chosen port into (e.g. `API_SERVER_PORT`).
    // A service that reads its port from a config file on disk may ignore the
    // process environment entirely. Confirm the override actually takes effect
    // before trusting dynamic ports with a given harness.
    #[serde(default)]
    pub env: Option<String>,
    /// CLI flag to inject the chosen port with (e.g. `--port`).
    #[serde(default)]
    pub arg: Option<String>,
}

/// How to confirm the harness is actually serving on its port.
#[derive(Clone, Debug, Deserialize)]
pub struct HealthSpec {
    /// `"http"` (GET `path`), `"ws"` (WebSocket probe), or `"process"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// HTTP path for `http` health (e.g. `/health`).
    #[serde(default)]
    pub path: Option<String>,
    /// WS method/probe hint for `ws` health, for services that expose no HTTP
    /// health path.
    #[serde(default)]
    pub probe: Option<String>,
}

/// A spawned harness the supervisor owns, with everything needed to restart it.
pub struct RunningHarness {
    pub child: Child,
    pub spec: LaunchSpec,
    pub port: u16,
}

/// Result of polling a supervised child for exit.
pub enum PollExit {
    /// Still running.
    Running,
    /// No entry for this id (nothing to supervise).
    Absent,
    /// Exited; carries what is needed to restart it.
    Exited { spec: LaunchSpec, port: u16 },
}

/// Outcome of a start request returned to the renderer.
#[derive(Serialize)]
pub struct StartOutcome {
    pub id: String,
    pub port: u16,
    /// True when we spawned and supervise the process.
    pub spawned: bool,
    /// True when an external instance already held the port and we connect
    /// to it instead of spawning (user-installed daemon).
    pub connected: bool,
}

/// Status of a supervised harness for `harness_status`.
#[derive(Serialize)]
pub struct HarnessStatus {
    pub id: String,
    pub running: bool,
    pub port: Option<u16>,
}

/// Windows creation flags: detach, own process group, no console window.
/// Same flags the sidecar spawn uses so no stray consoles appear.
#[cfg(target_os = "windows")]
fn apply_windows_flags(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn apply_windows_flags(_cmd: &mut Command) {}

/// Build the launch `Command` with the chosen port injected via env and/or arg.
fn build_command(spec: &LaunchSpec, port: u16) -> Command {
    let mut cmd = Command::new(&spec.command);
    for arg in &spec.args {
        cmd.arg(arg);
    }
    if let Some(flag) = &spec.port.arg {
        cmd.arg(flag);
        cmd.arg(port.to_string());
    }
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    for (key, val) in &spec.env {
        cmd.env(key, val);
    }
    if let Some(env_key) = &spec.port.env {
        cmd.env(env_key, port.to_string());
    }
    apply_windows_flags(&mut cmd);
    cmd
}

/// Per-harness lock file path (mirrors the sidecar lock-file convention).
fn lock_file(id: &str) -> std::path::PathBuf {
    paths::get_index_dir().join(format!("harness_{id}.lock"))
}

/// True if something is already accepting connections on `127.0.0.1:port`.
/// Used to detect a pre-existing external daemon before spawning.
pub fn port_in_use(port: u16) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, HEALTH_CONNECT_TIMEOUT).is_ok()
}

/// Run a single health probe per the spec's `health` block.
///
/// `http`: GET the path and accept any non-5xx status as "serving".
/// `ws`/`process`: liveness only — the caller confirms the supervised child is
/// alive for `process`. See the note on the `ws` arm below for why that is
/// weaker than it looks.
pub fn health_probe(spec: &LaunchSpec, port: u16) -> Result<(), String> {
    match spec.health.kind.as_str() {
        "http" => {
            let path = spec.health.path.as_deref().unwrap_or("/health");
            health_probe_http(port, path)
        }
        // A bare TCP connect only proves the port is bound, not that the
        // WebSocket endpoint completed its upgrade and is accepting messages.
        // For a service whose readiness means "finished its handshake", replace
        // this with a real WS upgrade probe; otherwise the supervisor will
        // report healthy while the first request still fails.
        "ws" => {
            let probe = spec.health.probe.as_deref().unwrap_or("status");
            if port_in_use(port) {
                log::debug!("ws health: port {port} up (probe hint '{probe}')");
                Ok(())
            } else {
                Err(format!("ws health: nothing listening on port {port}"))
            }
        }
        // Liveness is verified by the caller via the supervised child handle.
        "process" => Ok(()),
        other => Err(format!("unknown health type: {other}")),
    }
}

/// Minimal HTTP GET probe: connect, request, read the status line.
fn health_probe_http(port: u16, path: &str) -> Result<(), String> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("bad health addr: {e}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, HEALTH_CONNECT_TIMEOUT)
        .map_err(|e| format!("http health connect failed: {e}"))?;
    stream
        .set_read_timeout(Some(HEALTH_IO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(HEALTH_IO_TIMEOUT))
        .map_err(|e| e.to_string())?;

    let req =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("http health write failed: {e}"))?;

    let mut status_line = String::new();
    BufReader::new(stream)
        .read_line(&mut status_line)
        .map_err(|e| format!("http health read failed: {e}"))?;

    // Expect "HTTP/1.1 <code> <reason>". Treat any non-5xx as serving; a 401
    // (auth required) still proves the harness bound the port.
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(c) if c < 500 => Ok(()),
        Some(c) => Err(format!("http health returned {c}")),
        None => Err(format!("http health: unparseable response '{}'", status_line.trim())),
    }
}

/// Tracks restart attempts with capped backoff and a reset window.
/// Extracted from the monitor so the backoff schedule is unit-testable.
pub struct RestartTracker {
    attempts: usize,
    window_start: Instant,
}

impl Default for RestartTracker {
    fn default() -> Self {
        Self {
            attempts: 0,
            window_start: Instant::now(),
        }
    }
}

impl RestartTracker {
    /// Next backoff delay in seconds, or `None` once the cap is exhausted within
    /// the window. Resets the counter if the window has elapsed.
    pub fn next_delay(&mut self) -> Option<u64> {
        if self.window_start.elapsed() > HARNESS_RESTART_WINDOW {
            self.attempts = 0;
            self.window_start = Instant::now();
        }
        if self.attempts >= HARNESS_RESTART_DELAYS_SECS.len() {
            return None;
        }
        let delay = HARNESS_RESTART_DELAYS_SECS[self.attempts];
        self.attempts += 1;
        Some(delay)
    }
}

impl HarnessSupervisor {
    /// Spawn the harness process, record it, and write its lock file.
    /// Clears any prior `stopping` flag for this id.
    pub fn spawn_process(&self, id: &str, spec: &LaunchSpec, port: u16) -> Result<u32, String> {
        let mut cmd = build_command(spec, port);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn harness '{id}': {e}"))?;
        let pid = child.id();

        if let Err(e) = std::fs::write(lock_file(id), pid.to_string()) {
            log::warn!("Failed to write harness lock file for '{id}': {e}");
        }

        self.stopping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);

        // Insert under the children lock and re-check `shutting_down` while
        // holding it. stop_all() sets the flag and drains children under this
        // same lock, so a shutdown racing this spawn cannot orphan the child:
        // either we observe the flag here and kill it, or stop_all sees it in
        // the map and kills it during the drain. (TOCTOU fix: the monitor may
        // reach spawn_process after stop_all has already drained.)
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        if self.shutting_down.load(Ordering::SeqCst) {
            drop(children);
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(lock_file(id));
            return Err(format!(
                "Harness '{id}' not started: supervisor is shutting down"
            ));
        }
        children.insert(
            id.to_string(),
            RunningHarness {
                child,
                spec: spec.clone(),
                port,
            },
        );
        drop(children);
        log::info!("Harness '{id}' spawned (pid={pid}, port={port})");
        Ok(pid)
    }

    /// Poll the supervised child; on exit, remove it and return restart inputs.
    pub fn poll_exit(&self, id: &str) -> PollExit {
        let mut guard = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = guard.get_mut(id) else {
            return PollExit::Absent;
        };
        match entry.child.try_wait() {
            Ok(Some(status)) => {
                log::error!("Harness '{id}' exited unexpectedly (status: {status:?})");
                let spec = entry.spec.clone();
                let port = entry.port;
                guard.remove(id);
                // Crash path: the map entry is gone but the child's lock file
                // still names the now-dead PID. Remove it so a zombie can't be
                // mistaken for a live external daemon; a restart rewrites it.
                let _ = std::fs::remove_file(lock_file(id));
                PollExit::Exited { spec, port }
            }
            Ok(None) => PollExit::Running,
            Err(e) => {
                log::warn!("Failed to poll harness '{id}': {e}");
                PollExit::Running
            }
        }
    }

    /// True if the id is flagged as intentionally stopping.
    pub fn is_stopping(&self, id: &str) -> bool {
        self.stopping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }

    /// True if a live child is supervised for this id.
    pub fn is_running(&self, id: &str) -> bool {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }

    /// Port a supervised harness was launched on, if any.
    pub fn port_of(&self, id: &str) -> Option<u16> {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|h| h.port)
    }

    /// Status snapshot for `harness_status`.
    pub fn status(&self, id: &str) -> HarnessStatus {
        HarnessStatus {
            id: id.to_string(),
            running: self.is_running(id),
            port: self.port_of(id),
        }
    }

    /// Intentionally stop one harness: flag it (so the monitor won't restart),
    /// kill the child, and remove its lock file.
    pub fn stop(&self, id: &str) -> Result<(), String> {
        self.stopping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string());

        let entry = self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);

        if let Some(mut harness) = entry {
            let _ = harness.child.kill();
            let _ = harness.child.wait();
        }
        let _ = std::fs::remove_file(lock_file(id));
        log::info!("Harness '{id}' stopped");
        Ok(())
    }

    /// Stop every supervised harness (app exit). Sets `shutting_down` so any
    /// running monitors bail out without restarting.
    pub fn stop_all(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let mut guard = self.children.lock().unwrap_or_else(|e| e.into_inner());
        for (id, mut harness) in guard.drain() {
            let _ = harness.child.kill();
            let _ = harness.child.wait();
            let _ = std::fs::remove_file(lock_file(&id));
            log::info!("Harness '{id}' stopped on shutdown");
        }
    }
}

/// Supervise one harness child: poll for exit, restart with capped backoff,
/// emit `harness-status` events. Mirrors `spawn_sidecar_monitor` per harness id.
///
/// Emits `harness-status` with `{ id, status, attempt }` where status is
/// `"down"`, `"restarted"`, or `"failed"`.
pub fn spawn_harness_monitor(app: AppHandle, id: String) {
    tauri::async_runtime::spawn(async move {
        let mut tracker = RestartTracker::default();

        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let supervisor = app.state::<HarnessSupervisor>();
            if supervisor.shutting_down.load(Ordering::SeqCst) || supervisor.is_stopping(&id) {
                return;
            }

            let (spec, port) = match supervisor.poll_exit(&id) {
                PollExit::Running => continue,
                PollExit::Absent => return,
                PollExit::Exited { spec, port } => (spec, port),
            };

            if supervisor.shutting_down.load(Ordering::SeqCst) || supervisor.is_stopping(&id) {
                return;
            }

            let _ = app.emit(
                "harness-status",
                serde_json::json!({ "id": id, "status": "down" }),
            );

            loop {
                let Some(delay) = tracker.next_delay() else {
                    log::error!("Harness '{id}' crashed too many times, giving up");
                    // Give-up path: ensure no stale lock lingers (poll_exit already
                    // removed it on the crash, but a failed restart may have left one).
                    let _ = std::fs::remove_file(lock_file(&id));
                    let _ = app.emit(
                        "harness-status",
                        serde_json::json!({ "id": id, "status": "failed" }),
                    );
                    return;
                };
                tokio::time::sleep(Duration::from_secs(delay)).await;

                let supervisor = app.state::<HarnessSupervisor>();
                if supervisor.shutting_down.load(Ordering::SeqCst) || supervisor.is_stopping(&id) {
                    return;
                }

                match supervisor.spawn_process(&id, &spec, port) {
                    Ok(_) => {
                        log::info!("Harness '{id}' restarted");
                        let _ = app.emit(
                            "harness-status",
                            serde_json::json!({ "id": id, "status": "restarted" }),
                        );
                        break;
                    }
                    Err(e) => log::error!("Harness '{id}' restart failed: {e}"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::HarnessSupervisor;
    use std::net::TcpListener;

    /// A long-running dummy command (portable across Windows/Unix).
    fn long_spec() -> LaunchSpec {
        #[cfg(target_os = "windows")]
        let (command, args) = (
            "cmd".to_string(),
            vec!["/C".to_string(), "ping -n 30 127.0.0.1 > NUL".to_string()],
        );
        #[cfg(not(target_os = "windows"))]
        let (command, args) = ("sleep".to_string(), vec!["30".to_string()]);

        LaunchSpec {
            command,
            args,
            cwd: None,
            env: HashMap::new(),
            port: PortSpec {
                strategy: "dynamic".to_string(),
                value: None,
                env: None,
                arg: None,
            },
            health: HealthSpec {
                kind: "process".to_string(),
                path: None,
                probe: None,
            },
        }
    }

    /// A command that exits immediately.
    fn short_spec() -> LaunchSpec {
        #[cfg(target_os = "windows")]
        let (command, args) = ("cmd".to_string(), vec!["/C".to_string(), "exit".to_string()]);
        #[cfg(not(target_os = "windows"))]
        let (command, args) = ("sh".to_string(), vec!["-c".to_string(), "exit 0".to_string()]);

        let mut spec = long_spec();
        spec.command = command;
        spec.args = args;
        spec
    }

    #[test]
    fn test_allocate_free_port_is_bindable() {
        let port = paths::allocate_free_port().unwrap();
        assert!(port > 0);
        // Nothing should be holding a freshly allocated ephemeral port.
        assert!(!port_in_use(port), "freshly allocated port should be free");
    }

    #[test]
    fn test_port_in_use_detects_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_in_use(port), "bound port should read as in use");
        drop(listener);
    }

    #[test]
    fn test_build_command_injects_port_env_and_arg() {
        let mut spec = long_spec();
        spec.port.env = Some("API_SERVER_PORT".to_string());
        spec.port.arg = Some("--port".to_string());
        let cmd = build_command(&spec, 12345);

        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--port".to_string()));
        assert!(args.contains(&"12345".to_string()));

        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs
            .iter()
            .any(|(k, v)| k == "API_SERVER_PORT" && v.as_deref() == Some("12345")));
    }

    #[test]
    fn test_restart_tracker_backoff_then_gives_up() {
        let mut tracker = RestartTracker::default();
        assert_eq!(tracker.next_delay(), Some(1));
        assert_eq!(tracker.next_delay(), Some(2));
        assert_eq!(tracker.next_delay(), Some(5));
        assert_eq!(tracker.next_delay(), Some(5));
        assert_eq!(tracker.next_delay(), Some(5));
        assert_eq!(tracker.next_delay(), None, "cap exhausted within window");
    }

    #[test]
    fn test_http_health_ok_and_closed() {
        // Mock server that answers one request with 200.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 256];
                use std::io::Read;
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });

        assert!(health_probe_http(port, "/health").is_ok());
        handle.join().unwrap();

        // Nothing listening now -> connect fails.
        assert!(health_probe_http(port, "/health").is_err());
    }

    #[test]
    fn test_spawn_and_clean_stop() {
        let _env = paths::lock_data_dir_env();
        std::env::set_var("SIDECAR_DATA_DIR_OVERRIDE", std::env::temp_dir());
        std::fs::create_dir_all(paths::get_index_dir()).unwrap();

        let sup = HarnessSupervisor::default();
        let id = format!("test-stop-{}", std::process::id());
        sup.spawn_process(&id, &long_spec(), 4321).unwrap();

        assert!(sup.is_running(&id));
        assert!(lock_file(&id).exists(), "lock file should be written");
        assert_eq!(sup.port_of(&id), Some(4321));

        sup.stop(&id).unwrap();
        assert!(!sup.is_running(&id));
        assert!(!lock_file(&id).exists(), "lock file should be removed");
        assert!(sup.is_stopping(&id));
    }

    #[test]
    fn test_poll_exit_then_respawn() {
        let _env = paths::lock_data_dir_env();
        std::env::set_var("SIDECAR_DATA_DIR_OVERRIDE", std::env::temp_dir());
        std::fs::create_dir_all(paths::get_index_dir()).unwrap();

        let sup = HarnessSupervisor::default();
        let id = format!("test-respawn-{}", std::process::id());

        // Spawn a process that exits immediately, then observe the exit.
        sup.spawn_process(&id, &short_spec(), 4322).unwrap();
        let mut outcome = PollExit::Running;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(50));
            match sup.poll_exit(&id) {
                PollExit::Running => continue,
                other => {
                    outcome = other;
                    break;
                }
            }
        }
        match outcome {
            PollExit::Exited { port, .. } => assert_eq!(port, 4322),
            _ => panic!("expected the short-lived child to be reaped as Exited"),
        }
        assert!(!sup.is_running(&id), "exited child should be removed from map");

        // Simulate the monitor's restart with a long-running process.
        sup.spawn_process(&id, &long_spec(), 4322).unwrap();
        assert!(sup.is_running(&id));
        sup.stop(&id).unwrap();
    }

    #[test]
    fn test_stop_all_kills_children() {
        let _env = paths::lock_data_dir_env();
        std::env::set_var("SIDECAR_DATA_DIR_OVERRIDE", std::env::temp_dir());
        std::fs::create_dir_all(paths::get_index_dir()).unwrap();

        let sup = HarnessSupervisor::default();
        let id = format!("test-stopall-{}", std::process::id());
        sup.spawn_process(&id, &long_spec(), 4323).unwrap();
        assert!(sup.is_running(&id));

        sup.stop_all();
        assert!(!sup.is_running(&id));
        assert!(sup.shutting_down.load(Ordering::SeqCst));
    }
}
