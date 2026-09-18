//! Tauri managed state for the supervised processes.

use std::collections::{HashMap, HashSet};
use std::process::Child;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use crate::supervisor::RunningHarness;

/// App state supervising the persistent sidecar TCP server process.
///
/// Holds the child handle so the monitor task can detect unexpected exits and
/// restart the server. `shutting_down` is set on intentional shutdown (app
/// exit, pre-update cleanup) so the monitor does not restart a process we
/// killed on purpose.
#[derive(Default)]
pub struct SidecarSupervisor {
    pub child: Mutex<Option<Child>>,
    pub shutting_down: AtomicBool,
}

/// App state supervising N external harness processes.
///
/// Generalizes `SidecarSupervisor` into a map keyed by harness id. Methods live
/// in `supervisor.rs`. `children` holds the live child handles + their
/// launch spec/port (for restart); `stopping` marks ids we killed on purpose so
/// their monitor loop does not restart them; `shutting_down` stops all monitors
/// on app exit.
#[derive(Default)]
pub struct HarnessSupervisor {
    pub children: Mutex<HashMap<String, RunningHarness>>,
    pub stopping: Mutex<HashSet<String>>,
    pub shutting_down: AtomicBool,
}
