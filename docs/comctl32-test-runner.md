# `cargo test` dies with `STATUS_ENTRYPOINT_NOT_FOUND` before any test runs

This note stands on its own. If you arrived from a Tauri issue thread and have
never seen the rest of this repository, everything you need is here.

One clarification first, added after this repository was published. The template
you are looking at does **not** reproduce this crash. Its lib unit-test binary
imports nothing from `comctl32.dll`, verified by reading the binary's import
table, so plain `cargo test` passes here. The fix ships as a working reference
for apps that do hit it, and it is opted into per command rather than left on.

## The symptom

On `x86_64-pc-windows-msvc`, in a Tauri v2 app that depends on
`tauri-plugin-dialog` (directly or transitively), `cargo test` fails like this:

```
error: test failed, to rerun pass `--lib`

Caused by:
  process didn't exit successfully: `...\target\debug\deps\your_app_lib-<hash>.exe`
  (exit code: 0xc0000139, STATUS_ENTRYPOINT_NOT_FOUND)
```

No test name is printed, no `running 0 tests` line, no panic. The process dies
in the Windows loader before `main` is reached. `cargo build` and `cargo run`
are fine; only `cargo test` fails. The same code tests fine on Linux and macOS.

## The cause

`tauri-plugin-dialog` pulls in `rfd`, which imports `TaskDialogIndirect` from
`comctl32.dll`. That export exists only in Common Controls **version 6**.

Windows has two comctl32 libraries side by side. `C:\Windows\System32\comctl32.dll`
is version **5.82**, kept for compatibility, and it does **not** export
`TaskDialogIndirect`. Version 6 lives in the WinSxS store, and a process only
binds to it if its executable carries an application manifest declaring a
dependency on `Microsoft.Windows.Common-Controls` version `6.0.0.0`.

Your shipped `app.exe` has such a manifest: `tauri-build` generates one and links
it in through `resource.lib`, so the real application binds v6 and works.

**The cargo test binaries do not.** `cargo test` compiles the library crate into
its own separate executable (`your_app_lib-<hash>.exe`). That binary is produced
by rustc without `build.rs`'s linked resources, so it has no `RT_MANIFEST`
resource at all. With no manifest, the loader resolves `comctl32.dll` to the
System32 v5.82 copy, fails to find `TaskDialogIndirect` among its exports, and
terminates the process with `STATUS_ENTRYPOINT_NOT_FOUND` - before a single test
function runs.

It is an import-resolution failure at load time, which is why nothing in your
test code appears in the output and why nothing you change in your test code
fixes it.

## What does not work

`cargo:rustc-link-arg-tests=/MANIFEST:EMBED ...` in `build.rs` is the obvious
fix and it does not reach this binary. `rustc-link-arg-tests` applies to
*integration* test binaries (`tests/*.rs`), not to the **lib unit-test binary**
that `cargo test --lib` produces from `#[cfg(test)]` modules inside `src/`. That
is the binary that crashes, so the flag never touches it. `rustc-link-arg` in
its unscoped form is rejected for this target configuration.

Marking the tests `#[ignore]`, or moving them out of `src/` into `tests/`, both
"work" in the sense that the crash stops - by not running the code. Neither is
a fix.

## The fix: a cargo runner that embeds the manifest

Cargo lets you interpose a *runner* between itself and any test or binary it is
about to execute. The runner receives the executable path plus the test
arguments, and is responsible for running it. That is the one place where the
binary exists, is about to be loaded, and can still be patched.

Four files, all in this repository under `src-tauri/.cargo/`:

**`test-runner.toml`** - registers the runner, scoped to one target triple:

```toml
[target.x86_64-pc-windows-msvc]
runner = ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ".cargo\\run-test.ps1"]
```

You pass it explicitly, and only when testing:

```powershell
cargo test --config .cargo\test-runner.toml
```

**Do not put this key in `.cargo/config.toml`.** That was this repository's first
attempt and it is wrong. Cargo applies `runner` to `cargo run` as well as `cargo
test`, so the key is always live: `tauri dev` then launches your application
through `powershell.exe`, and because `tauri dev` pipes cargo's stdio rather than
handing it a console, Windows allocates a fresh one. The result is an empty
terminal window sitting next to your app every time you develop. Adding
`-WindowStyle Hidden` does not fix it, because on Windows 11 the console belongs
to Windows Terminal rather than to the PowerShell process. Passing the config per
command keeps `cargo run` completely untouched.

The target scope matters too. A `cargo test` run from WSL or Linux CI targets
`x86_64-unknown-linux-gnu`, never matches this key, and is completely
unaffected - no PowerShell, no `mt.exe`, no manifest.

**`comctl-v6.xml`** - a minimal manifest declaring the v6 dependency:

```xml
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
```

It is named `.xml` rather than `.manifest` deliberately: a `*.manifest` line in
a `.gitignore` (common in Windows repositories, and present in several Tauri
templates) would silently swallow it, and the failure that produces looks
exactly like the original bug.

**`run-test.ps1`** - locates `mt.exe` from the newest installed Windows SDK,
checks whether the target executable already has an `RT_MANIFEST` at resource
id 1, embeds `comctl-v6.xml` **only when it does not**, then runs the
executable and propagates its exit code.

The "only when absent" probe is what makes the patching safe: `cargo run`
executes the real `app.exe`, which already carries Tauri's full manifest, so it
is detected, left untouched, and launched unmodified. Only the manifest-less test
binaries get patched. That protects the binary, which is not the same as being
free to leave the runner registered globally; see the console-window problem
above.

The SDK lookup result is cached in a file under `%TEMP%` so the directory scan
does not repeat on every test binary in a run.

If `mt.exe` is not found, the runner prints a warning naming the likely crash
and runs the binary anyway, rather than failing silently.

## Reproducing and verifying

Reproduce: create a Tauri v2 app, add `tauri-plugin-dialog = "2"`, put any
`#[test]` inside `src/`, and run `cargo test` on Windows.

Verify the fix: with the `.cargo/` files in place, `cargo test --config
.cargo\test-runner.toml` runs the tests normally. To confirm the runner actually fired rather than the binary
happening to load, delete the `%TEMP%` cache file the script writes and check
that it reappears after a test run.

## Requirements

- Windows 10/11 SDK installed (for `mt.exe`). It ships with the Visual Studio
  C++ build tools that the MSVC Rust toolchain already requires.
- PowerShell 5.1 or later, which is present on any supported Windows.

## Applicability

This is not specific to `tauri-plugin-dialog`. Any crate whose test binary
imports a symbol that exists only in a side-by-side assembly version will fail
the same way, and the same runner fixes it - change the manifest contents to
declare whatever assembly that dependency needs.
