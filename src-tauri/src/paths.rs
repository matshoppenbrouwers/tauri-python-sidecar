//! Sidecar path resolution.
//!
//! Handles the difference between development mode (Python interpreter) and
//! production mode (frozen sidecar executable).
//!
//! # Development Mode
//! Uses `python -m sidecar.loader <mode>` from the project root directory.
//!
//! # Production Mode
//! Uses the frozen sidecar executable bundled with the Tauri app.
//! Located at `<app_dir>/py-sidecar.exe` (Tauri strips target triple suffix when installing).

use std::path::PathBuf;
use std::process::Command;

/// Sidecar execution configuration
pub struct SidecarConfig {
    /// Path to the executable (python in dev, py-sidecar.exe in prod)
    pub executable: PathBuf,
    /// Arguments to pass before the mode argument
    pub base_args: Vec<String>,
    /// Working directory for the process
    pub working_dir: Option<PathBuf>,
    /// Whether we're in development mode
    pub is_dev: bool,
}

impl SidecarConfig {
    /// Create a Command for sidecar-server mode (persistent TCP server)
    pub fn sidecar_server_command(&self) -> Command {
        self.command_for_mode("sidecar-server")
    }

    /// Create a Command for a specific mode.
    ///
    /// Kept generic: a real app usually grows more than one mode (a background
    /// worker, a sync process), and every one of them is the same frozen binary
    /// with a different first argument. `loader.py` dispatches on it.
    pub fn command_for_mode(&self, mode: &str) -> Command {
        let mut cmd = Command::new(&self.executable);

        for arg in &self.base_args {
            cmd.arg(arg);
        }

        cmd.arg(mode);

        if let Some(ref dir) = self.working_dir {
            cmd.current_dir(dir);
        }

        cmd
    }
}

/// Get the sidecar configuration for the current environment.
///
/// # Returns
/// A `SidecarConfig` with the appropriate executable path and arguments, or a
/// readable error when the production sidecar binary or data directory is
/// unavailable (corrupted/incomplete bundle).
pub fn get_sidecar_config() -> Result<SidecarConfig, String> {
    // SIMPLICITY: Trust the compiler.
    // debug_assertions = true  -> We are running `tauri dev` or `cargo run`
    // debug_assertions = false -> We are running `tauri build` (production)
    if cfg!(debug_assertions) {
        Ok(get_dev_config())
    } else {
        get_prod_config()
    }
}

/// Get project root directory for development mode.
fn get_project_root_dev() -> Option<String> {
    // Try SIDECAR_PROJECT_ROOT env var first
    if let Ok(root) = std::env::var("SIDECAR_PROJECT_ROOT") {
        return Some(root);
    }

    // Fall back to CARGO_MANIFEST_DIR parent (only works when running from cargo)
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent()
        .and_then(|p| p.to_str())
        .map(|s| s.to_string())
}

/// Locate the interpreter to use in development mode.
///
/// The project's own `.venv` wins over whatever `python` happens to be first on
/// `PATH`. This is not a nicety: the sidecar needs its dependencies installed,
/// and on a developer machine with several interpreters (a system Python, a
/// conda base, another project's venv exported into the shell) the bare name
/// resolves to an interpreter that cannot import `filelock`. The sidecar then
/// dies on import, the supervisor restarts it five times, and the failure reads
/// as "the supervisor is broken" rather than "wrong Python". Falling back to
/// the bare name keeps a global-install workflow working.
fn dev_python_exe(project_root: &str) -> PathBuf {
    #[cfg(target_os = "windows")]
    let (venv_relative, fallback) = ("\\.venv\\Scripts\\python.exe", "python");

    #[cfg(not(target_os = "windows"))]
    let (venv_relative, fallback) = ("/.venv/bin/python", "python3");

    let venv_python = PathBuf::from(format!("{}{}", project_root, venv_relative));
    if venv_python.exists() {
        return venv_python;
    }
    PathBuf::from(fallback)
}

/// Get sidecar config for development mode.
fn get_dev_config() -> SidecarConfig {
    let project_root = get_project_root_dev().unwrap_or_else(|| ".".to_string());

    SidecarConfig {
        executable: dev_python_exe(&project_root),
        base_args: vec!["-m".to_string(), "sidecar.loader".to_string()],
        working_dir: Some(PathBuf::from(&project_root)),
        is_dev: true,
    }
}

/// Get sidecar config for production mode.
///
/// In production, the sidecar is bundled as a single onefile executable.
/// Tauri places it at: `<app_dir>/py-sidecar.exe`
/// Note: Tauri strips the target triple suffix when installing!
///
/// # Errors
/// Returns a readable error if the sidecar executable is not found (corrupted
/// bundle) or the data directory cannot be created.
fn get_prod_config() -> Result<SidecarConfig, String> {
    // Get the app's binary directory
    let exe_path =
        std::env::current_exe().map_err(|e| format!("Failed to get current exe path: {}", e))?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| format!("Failed to get exe directory for '{}'", exe_path.display()))?;

    // Tauri strips target triple suffix when installing!
    // Build time: binaries/py-sidecar-x86_64-pc-windows-msvc.exe
    // Install time: <app_dir>/py-sidecar.exe
    #[cfg(target_os = "windows")]
    let sidecar_exe = exe_dir.join("py-sidecar.exe");

    #[cfg(not(target_os = "windows"))]
    let sidecar_exe = exe_dir.join("py-sidecar");

    // Validate sidecar exists - fail fast with clear error message
    if !sidecar_exe.exists() {
        return Err(format!(
            "Sidecar executable not found at '{}'. \
            The application bundle may be corrupted or incomplete.",
            sidecar_exe.display()
        ));
    }

    // Production mode: working directory is the DATA directory, not exe directory
    let data_dir = get_data_dir();

    // Create data directory if it doesn't exist
    if !data_dir.exists() {
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            format!(
                "Failed to create data directory '{}': {}",
                data_dir.display(),
                e
            )
        })?;
    }

    Ok(SidecarConfig {
        executable: sidecar_exe,
        base_args: vec![],
        working_dir: Some(data_dir), // Changed from exe_dir
        is_dev: false,
    })
}

/// Get the data directory for user data (config, database, logs).
///
/// Uses standard platform-specific directories:
/// - Windows: `%APPDATA%\TauriPythonSidecar`
/// - macOS: `~/Library/Application Support/TauriPythonSidecar`
/// - Linux: `~/.config/tauri-python-sidecar`
///
/// In development mode, falls back to project root.
///
/// Can be overridden with `SIDECAR_DATA_DIR_OVERRIDE` for test isolation.
pub fn get_data_dir() -> PathBuf {
    // Test isolation: allow override via environment variable
    if let Ok(override_dir) = std::env::var("SIDECAR_DATA_DIR_OVERRIDE") {
        return PathBuf::from(override_dir);
    }

    if cfg!(debug_assertions) {
        // Development: use project root
        get_project_root_dev()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        // Production: use platform-specific data directory
        #[cfg(target_os = "windows")]
        {
            // %APPDATA%\TauriPythonSidecar
            std::env::var("APPDATA")
                .map(|s| PathBuf::from(s).join("TauriPythonSidecar"))
                .unwrap_or_else(|_| PathBuf::from("."))
        }

        #[cfg(target_os = "macos")]
        {
            // ~/Library/Application Support/TauriPythonSidecar
            dirs::data_dir()
                .map(|p| p.join("TauriPythonSidecar"))
                .unwrap_or_else(|| PathBuf::from("."))
        }

        #[cfg(target_os = "linux")]
        {
            // ~/.config/tauri-python-sidecar
            dirs::config_dir()
                .map(|p| p.join("tauri-python-sidecar"))
                .unwrap_or_else(|| PathBuf::from("."))
        }
    }
}

/// Get the index directory for database and lock files.
pub fn get_index_dir() -> PathBuf {
    get_data_dir().join(".index")
}

/// Allocate a free loopback port by binding `127.0.0.1:0` and reading the port
/// the OS assigned, then dropping the listener.
///
/// This is only a *candidate*: bind-then-drop is racy (TOCTOU), so callers MUST
/// confirm the process actually came up on it via a health check after launch —
/// never rely on the allocation alone (the `bind :0` helper in
/// `sidecar_client.rs` is a test-only guaranteed-closed port, not this).
pub fn allocate_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Failed to allocate free port: {e}"))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| format!("Failed to read allocated port: {e}"))
}

/// Serializes tests that mutate the process-global `SIDECAR_DATA_DIR_OVERRIDE`
/// env var. `cargo test` runs tests in parallel by default, so without this any
/// two tests racing `set_var`/`remove_var` make `get_data_dir()` resolve
/// inconsistently mid-test. Every test that sets or removes that var must hold
/// this guard for its whole body. Poison is ignored (a panicking test must not
/// wedge the rest).
#[cfg(test)]
static DATA_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the data-dir env guard; hold it for the whole test body. Poison is
/// ignored so one panicking test can't wedge the rest.
#[cfg(test)]
pub(crate) fn lock_data_dir_env() -> std::sync::MutexGuard<'static, ()> {
    DATA_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sidecar_config_mode_args() {
        let config = get_dev_config();

        let cmd = config.command_for_mode("sidecar-server");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();

        // Should have: -m, sidecar.loader, sidecar-server
        assert!(args.contains(&"-m"));
        assert!(args.contains(&"sidecar.loader"));
        assert!(args.contains(&"sidecar-server"));
    }

    #[test]
    fn test_allocate_free_port_returns_bindable_port() {
        let port = allocate_free_port().unwrap();
        assert!(port > 0, "allocated port should be non-zero");
        // The port is free again after allocation, so we can bind it.
        let listener = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(listener.is_ok(), "allocated port should be bindable");
    }

    #[test]
    fn test_data_dir_not_empty() {
        let data_dir = get_data_dir();
        // In test mode we're in dev, so this should be project root
        assert!(!data_dir.as_os_str().is_empty());
    }
}
