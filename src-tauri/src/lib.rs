//! Library root for the tauri-python-sidecar template.
//!
//! Scaffold stage: the supervision core (`supervisor`), the authenticated TCP
//! client (`sidecar_client`) and path/port resolution (`paths`) are in place and
//! unit-tested. The real `run()` — sidecar spawn, monitor loop with capped
//! backoff, graceful shutdown and SQLite WAL recovery — replaces this stub in
//! the next step.

pub mod paths;
pub mod sidecar_client;
pub mod state;
pub mod supervisor;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state::SidecarSupervisor::default())
        .manage(state::HarnessSupervisor::default())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
