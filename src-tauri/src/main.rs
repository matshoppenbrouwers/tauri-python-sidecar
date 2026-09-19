// No console subsystem, in debug builds as well as release.
//
// Tauri's template applies this only to release, via
// `cfg_attr(not(debug_assertions), ...)`, which leaves the debug binary a
// console application. That is invisible when you run `cargo run` from a
// terminal, because the app attaches to the one already there. It is not
// invisible under `tauri dev`: tauri pipes cargo's stdio so it can prefix the
// output, the app inherits pipes rather than a console, and Windows allocates a
// console for it. You get an empty terminal window beside the app on every dev
// run, empty because the app's output went to the pipe.
//
// Nothing is lost by suppressing it here. Log output still reaches stdout, and
// `tauri dev` reprints it in your terminal; tauri-plugin-log also writes to
// %LOCALAPPDATA%\<identifier>\logs.
#![windows_subsystem = "windows"]

fn main() {
    tauri_python_sidecar_lib::run()
}
