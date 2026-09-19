# tauri-python-sidecar

A Tauri v2 desktop app that runs a Python process as a supervised sidecar, with
the parts that usually get left out: an authenticated local transport, SQLite
migrations that survive several processes starting at once, crash recovery,
Nuitka packaging, code signing and auto-update.

Windows is the platform this was built and verified on. The Rust is
cross-platform where the monorepo it came from was; the packaging and the
process handling are Windows-specific and say so.

MIT licensed. Take what you need.

---

## 1. Why this exists

Running Python behind a Tauri app is a common shape and a well-documented one,
up to the point where it has to work on someone else's machine. Seven things
have to hold at once:

1. **Process supervision** - the sidecar dies; something notices and restarts it
   with backoff, and gives up loudly rather than retrying forever.
2. **An authenticated local transport** - a TCP port on loopback is reachable by
   anything else running as the same user.
3. **Cross-process schema migrations** - the sidecar, a worker and a one-shot
   invocation can all open the same SQLite file within the same second at app
   launch.
4. **Crash recovery** - stale PID locks, a write-ahead log left behind by a hard
   kill, a detached process that outlived the window closing.
5. **Freezing Python into a binary** - Nuitka, and the specific flags that make
   it fit in memory and not ship a licence you did not intend.
6. **Code signing** - of the sidecar executable, not just the app.
7. **Auto-update** - that takes the running sidecar down before overwriting it,
   and does not delete the user's data on the way through.

A survey of the published examples found none that combines even three:

- [`dieharders/example-tauri-v2-python-server-sidecar`](https://github.com/dieharders/example-tauri-v2-python-server-sidecar)
 - PyInstaller, hello-world scope. Its own README's TODO list asks for the
  "multi-sidecar manager" that does not exist.
- [`fudanglp/tauri-fastapi-full-stack-template`](https://github.com/fudanglp/tauri-fastapi-full-stack-template)
 - describes itself as production-ready; auth is off by default, and there is
  no crash recovery, no migration locking, no signing and no updater.
- Nothing at all on Nuitka with Tauri. Every guide is PyInstaller.

The demand is on the record rather than assumed.
[tauri-apps/plugins-workspace#3062](https://github.com/tauri-apps/plugins-workspace/issues/3062)
asks for precisely this supervision layer - health checks and automatic restart
with backoff - and has been open and unimplemented since October 2025.
[tauri-apps/tauri#7381](https://github.com/tauri-apps/tauri/issues/7381) records
sidecar signing as a known gap.

`src-tauri/src/supervisor.rs` in this repository is a direct answer to the
first of those.

### The comments are the point

This is extracted from a shipped application, and what makes it worth cloning is
not the line count - it is the annotated reasoning attached to each piece. Why
the port allocator has a documented TOCTOU caveat. Why a successful WAL
checkpoint still needs a `SELECT 1` afterwards. Why `--python-flag=no_annotations`
looks free and is not. Why the restart ladder is capped at five and resets after
five minutes.

If you strip those comments to tidy the code, you have thrown away the part that
took the time.

---

## 2. Architecture

```
  ┌─────────────────────────── Tauri app (Rust) ───────────────────────────┐
  │                                                                        │
  │  lib.rs                                                                │
  │    ├─ startup: sweep stale PID locks ─► checkpoint stale SQLite WAL     │
  │    ├─ spawn sidecar ──► monitor thread ──► backoff [1,2,5,5,5]s         │
  │    │                        │              reset after 300s healthy     │
  │    │                        └─► emit `sidecar-status` to the webview     │
  │    └─ RunEvent::Exit: graceful taskkill ─► poll ─► /F                    │
  │                                                                        │
  │  commands/sidecar.rs      ALLOWED_METHODS = {ping, echo, get_status}    │
  │    └─ renderer allowlist; TCP first, one-shot spawn only on connect fail│
  │                                                                        │
  │  sidecar_client.rs        TCP ─► auth line ─► one JSON-RPC line ─► one  │
  │                           response line. Timeout ladder, connect-error  │
  │                           prefix so "busy" is never mistaken for "down" │
  └────────────────────────────────┬───────────────────────────────────────┘
                                   │  127.0.0.1:<port>, newline-delimited JSON
                                   │  first line: {"type":"auth","token":"…"}
  ┌────────────────────────────────┴───────────────────────────────────────┐
  │  Python sidecar (sidecar/)                                             │
  │    loader.py     freeze_support, late imports, crash log to file       │
  │    server.py     asyncio TCP server, token handshake, PID lock,        │
  │                  stdout/stderr redirect (a frozen GUI exe has none)    │
  │    dispatcher.py method registry + JSON-RPC envelope                   │
  │    handlers.py   ping · echo · get_status                              │
  │    storage/      migration runner: sentinel ─► file lock ─► re-check   │
  │                  ─► apply, so N processes apply it exactly once        │
  └────────────────────────────────┬───────────────────────────────────────┘
                                   │
                    %APPDATA%\TauriPythonSidecar\.index\
                      sidecar.db · sidecar_server.token (0600) · *.lock · logs

  packaging/   nuitka-build.py ─► py-sidecar-x86_64-pc-windows-msvc.exe
               sign.ps1 (AzureSignTool, soft-fails unconfigured)
               generate_latest_json.py ─► Tauri updater manifest
  .github/     release.yml: version gate ─► build ─► sign ─► publish
```

Both sides derive their paths the same way on purpose: Rust's `get_index_dir()`
and Python's `SidecarConfig.index_dir` must resolve to the same directory, or
the client looks for the session token where the server never wrote it. The
supervisor injects `SIDECAR_DATA_DIR` at spawn so they cannot drift.

---

## 3. Should you use this pattern at all

### Versus PyTauri

[PyTauri](https://github.com/pytauri/pytauri) embeds Python in the Tauri process
through PyO3. It is a different pattern, not a competing implementation of this
one, and for a lot of applications it is the better choice: one process, no
transport, no supervision, direct calls.

Choose the sidecar pattern when one of these is true:

- **You want process isolation.** A segfault in a native extension, an
  out-of-memory kill, or a C library that calls `exit()` takes down a sidecar.
  In an embedded runtime it takes down your whole application, window and all.
- **You have an existing Python codebase.** A sidecar takes the code as it is.
  Embedding means fitting it to PyO3's expectations: the GIL, the object
  conversions, the build.
- **You want crash resilience to be visible.** The demo in this repository kills
  the Python process and the app recovers in about a second with the UI showing
  what happened. That property is only available when the thing that crashed was
  not you.

Choose PyTauri when you want the lowest call latency, a single deployable
process and no IPC to reason about, and your Python is well-behaved enough that
its crashes may as well be yours.

Honest note on momentum: PyTauri is the more actively developed project and its
audience is growing. If you are starting fresh with no constraints, look at it
first.

### Versus FastAPI over HTTP

The usual sidecar advice is to run FastAPI or Flask and speak HTTP to it. This
template uses raw TCP with newline-delimited JSON-RPC, and the honest accounting
is:

**What raw TCP costs you.** No OpenAPI schema and no generated clients. No
`curl` against a running sidecar, no browser devtools network tab, no Postman - 
debugging means a Python script that speaks the line protocol. No middleware
ecosystem: CORS, rate limiting, compression and request logging are all yours to
write if you want them. No streaming primitives beyond what
`send_request_with_progress` implements by hand. Very few developers have this
protocol in their fingers, and every contributor has to learn it. It is about
600 lines of transport code you now own, against a dependency you would not.

**What it buys.** The dependency footprint is the standard library plus
`filelock` - which is why the Nuitka include list is three packages and the
binary is small, and why there are no compilation surprises from a web
framework's dynamic imports. The message contract is one line in, one line out,
which makes the "is the server down or just busy?" distinction exact:
`CONNECT_ERROR_PREFIX` marks connect failures as safe to fall back on, while a
read timeout means the server is working and spawning a second process would
double-execute the request. Over an HTTP client that distinction is muddier than
it looks. And there is no HTTP server bound on the machine for another
application to find and probe.

**A fair summary.** If your sidecar is mainly a request/response API and you
value tooling and familiarity, FastAPI over loopback HTTP is a perfectly good
choice, and the supervision, migration, packaging and updater parts of this
repository transfer to it unchanged - the transport is one replaceable layer.
The transport was kept verbatim here because it was working, shipped code and
rewriting it would have meant publishing something unproven.

---

## 4. Quickstart

### Prerequisites

- Windows 10 or 11
- Rust, MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`)
- Windows 10/11 SDK, only if you adopt the optional comctl32 test runner; see
  [docs/comctl32-test-runner.md](docs/comctl32-test-runner.md)
- Python 3.10 or later
- Node 18 or later, with pnpm. If `pnpm` is not on `PATH`, enable it with
  `corepack enable pnpm` on Node 18 to 24, or install it directly with
  `npm install -g pnpm` on Node 25 and later, which no longer ships corepack

### Development

```powershell
git clone https://github.com/matshoppenbrouwers/tauri-python-sidecar
cd tauri-python-sidecar

py -3 -m venv .venv
.venv\Scripts\python.exe -m pip install -e ".[dev]"

pnpm install
pnpm dev:app
```

Two things about those commands are not interchangeable with the obvious
alternatives:

**Use `.venv\Scripts\python.exe` explicitly, never a bare `python`.** On a
machine with several Python installations, `python` resolves to whichever one is
first on `PATH`, which is usually not this project's virtual environment. The
sidecar then fails to import `filelock`, dies at startup, and the supervisor
burns all five restart attempts - which presents as "the supervisor is broken"
rather than "the wrong interpreter ran". `src-tauri/src/paths.rs` prefers
`.venv/Scripts/python.exe` when it exists for exactly this reason.

**Use `pnpm dev:app`, not `pnpm tauri dev`.** `tauri.conf.json` declares
`bundle.externalBin: ["binaries/py-sidecar"]`, and Tauri's build script verifies
that the file exists - in development as well as in a release build. On a fresh
clone it does not exist until you have run Nuitka, which takes minutes and needs
MSVC. `pnpm dev:app` runs `packaging/dev_placeholder.py` first, which creates an
empty placeholder; development never executes it, because `paths.rs` branches on
`debug_assertions` and runs your `.venv` Python instead. This is deliberately
not wired into `pnpm tauri build`.

### The demo

The window shows a status badge, the sidecar's PID, its schema version and a
note count, an echo box, and a supervisor log.

1. Type something into the echo box and send it. The round trip proves the token
   handshake connected and the JSON-RPC transport works.
2. `taskkill /F /PID <the PID shown in the UI>`.
3. Watch the status go to `down`, wait about a second, and watch it come back as
   `restarted` with a **different PID**. The log shows the backoff delay.

That is the pitch. Everything else in this repository exists to make that keep
working after packaging, signing and an auto-update.

### Tests

```powershell
.venv\Scripts\python.exe -m pytest -q

.venv\Scripts\python.exe packaging\dev_placeholder.py
cd src-tauri; cargo test
```

They also need `packaging/dev_placeholder.py` to have run at least once in the
clone. `cargo test` builds the crate, which runs Tauri's build script, which
refuses to proceed while the `externalBin` file named in `tauri.conf.json` is
missing - and a fresh clone has no sidecar binary yet. The placeholder is an
empty file that satisfies that check and is never executed; the script's
docstring explains why it is deliberately not wired into the release build.
`pnpm dev:app` runs it for you, so this line is only needed when `cargo test` is
the first thing you do after cloning.

### Release build

```powershell
.venv\Scripts\python.exe -m pip install -r packaging\build-requirements.txt
.venv\Scripts\python.exe packaging\nuitka-build.py
pnpm tauri build
```

The Nuitka build takes around four minutes cold and produces
`src-tauri/binaries/py-sidecar-x86_64-pc-windows-msvc.exe`. `pnpm tauri build`
bundles it into an NSIS installer. Signing is skipped when no credentials are
configured - `packaging/sign.ps1` exits 0 with a warning on purpose, so a clone
without a certificate still builds.

### Troubleshooting

**`cargo test` exits immediately with `STATUS_ENTRYPOINT_NOT_FOUND`.** This
crate's tests do not trigger it, so you are seeing it after adding a dependency
whose dialog code reaches the test binary. Run
`cargo test --config .cargo\test-runner.toml`, which needs the Windows 10/11 SDK
for `mt.exe`. See [docs/comctl32-test-runner.md](docs/comctl32-test-runner.md).

**An empty console window opens next to the app under `pnpm dev:app`.** Something
registered a cargo `runner` in `.cargo/config.toml`. Cargo applies it to `cargo
run` too, so the app launches through `powershell.exe` and Windows gives it a
console. Move the key to `.cargo/test-runner.toml` and pass it only when testing.

**`tauri dev` fails with `resource path binaries\py-sidecar-…exe doesn't exist`.**
You ran `pnpm tauri dev` instead of `pnpm dev:app`. Run
`python packaging/dev_placeholder.py` once, or use `pnpm dev:app`.

**The sidecar starts and dies five times, then `failed`.** Almost always the
wrong Python. Check the log line naming the interpreter, and confirm
`.venv\Scripts\python.exe -c "import filelock"` succeeds.

**The PID in the supervisor log is not the PID holding the lock file.** If your
`.venv` was created from another virtual environment rather than from a system
Python, its `python.exe` is a launcher that re-executes itself, so there are two
processes. Supervision still works - killing the real server makes the launcher
exit, which the supervisor sees - but the PIDs will not match. A virtual
environment created directly from a system Python has no such indirection.

**`sidecar/shutdown.py` resolves `.index/` relative to the working directory**,
while everything else resolves it through `SIDECAR_DATA_DIR`. It is kept verbatim
from the source application, where it works because the supervisor sets the
child's working directory. If you spawn the sidecar from somewhere else, make
this file take its path from `SidecarConfig` like the rest.

### Making it yours

Search for `TODO_YOUR_`. What is left is deliberately yours to supply: the
updater public key and endpoint in `src-tauri/tauri.conf.json`, the signing
description in `packaging/sign.ps1`, and the company and copyright strings
stamped into the binary by `packaging/nuitka-build.py`.

Then change the things that still name this repository: the `authors`,
`repository` and `homepage` fields in `src-tauri/Cargo.toml` and
`pyproject.toml`, `REPO_URL` in `packaging/generate_latest_json.py`, the
copyright holder in `LICENSE`, `identifier` in `tauri.conf.json`, and the data
directory name in `src-tauri/src/paths.rs`.

**The auto-updater ships wired but switched off**, which is intentional: with
`createUpdaterArtifacts: true` and no signing key set, `pnpm tauri build` hard
fails, and every fresh clone would fail that way. To turn it on:

1. `pnpm tauri signer generate -w ~/.tauri/myapp.key`
2. Put the public key in `tauri.conf.json` under `plugins.updater.pubkey`, and
   the real release URL in `endpoints`.
3. Set `createUpdaterArtifacts: true`, and set `TAURI_SIGNING_PRIVATE_KEY` in
   the build environment.

`ui/src/hooks/useUpdater.ts` holds the update flow and is deliberately not
imported by `App.tsx` - the demo is about crash recovery. Read it for the
ordering it enforces: take a backup, **abort the whole update if the backup
failed**, kill the sidecars, then install. The backup call itself is a
documented TypeScript stub; the comment names the exact `invoke` that replaces
it and what the Rust side has to guarantee.

---

## 5. Component tour

### `src-tauri/src/lib.rs` - supervision, sweep, recovery

The monitor thread restarts the sidecar on unexpected exit with the delays
`[1, 2, 5, 5, 5]` seconds, then emits `failed` and stops. The ladder is capped
rather than exponential-forever because a sidecar that dies is almost always
either transiently unlucky - a port still in `TIME_WAIT`, a database still held
by the process that just died - in which case a second or two is enough, or
broken in a way that waiting will not fix, in which case the user needs to be
told rather than left watching an app retry silently for ten minutes.

The attempt counter resets after 300 seconds of health. Without that, an app
left open for a week would exhaust its five attempts on five unrelated crashes
months apart and then never restart again.

At startup it sweeps stale PID locks and checkpoints a stale write-ahead log.
The `WORKERS` const registry is what those sweeps walk, so supervising a second
process is a one-line change rather than four edits scattered through the file.

On a hard kill of the app - not a window close - the detached sidecar survives,
and that startup sweep is what cleans it up on the next launch.

That sweep is also the one place this pattern can do real damage, so it is worth
copying carefully. The lock file records the worker's **image name** next to its
PID, and the sweep kills only when the live process still carries that name.
Checking liveness alone is not enough: an operating system reuses a PID as soon
as it is free, so by the next launch the number in a crashed run's lock file may
belong to something else entirely, and a blind `taskkill /F` would take out a
stranger's program. A lock file with no recorded name is not killed either; a
leftover worker is a recoverable annoyance, killing the wrong process is not.

### `src-tauri/src/supervisor.rs` - the general case

A map of supervised child processes keyed by id, with declarative launch specs,
dynamic port allocation and injection, HTTP/WebSocket/process health probes and
a capped restart tracker. Twelve unit tests. The template itself supervises only
the Python sidecar; this file is here because it is the shape the general case
takes, and because it is the answer to plugins-workspace#3062.

The spawn path is TOCTOU-safe against shutdown: it re-checks the shutdown flag
after acquiring the lock, so a process cannot be spawned into an app that is
already exiting.

### `src-tauri/src/paths.rs` - where everything lives

`allocate_free_port()` binds port 0, reads the assignment and releases it. The
window between releasing and the child binding is a real race, documented rather
than papered over: the health check after spawn is what actually confirms the
port was still free.

`DATA_DIR_ENV_LOCK` serializes tests that mutate the process-global data
directory override. It exists because parallel tests were flaking on it.

### `src-tauri/src/sidecar_client.rs` and `commands/sidecar.rs`

The client sends the auth line, then one JSON-RPC line, and reads one response
line. `CONNECT_ERROR_PREFIX` marks the errors that mean "nothing is listening",
which is the only case where the command layer falls back to spawning a one-shot
process. A read timeout means the server **is** processing the request; spawning
then would execute it twice.

`ALLOWED_METHODS` is a renderer-facing allowlist. Adding a method means editing
both this constant and `sidecar/handlers.py`, which is deliberate: it keeps a
compromised or careless frontend from reaching arbitrary sidecar internals.

`LONG_RUNNING_METHODS` is empty in the template, with a comment explaining what
belongs in it and why raising the global timeout is the wrong fix.

### `sidecar/server.py` - the transport

Token handshake ([docs/auth-handshake.md](docs/auth-handshake.md)), PID lock, and
a `stdout`/`stderr` redirect to a log file. That redirect is not optional: the
frozen binary is built with the console disabled so it does not flash a black
window on every restart, which leaves it with no valid standard streams.

### `sidecar/storage/` - migrations under concurrency

`init_schema` is a sentinel fast path, then a cross-process file lock, then a
re-check inside the lock, then apply. Several processes opening the same
database within the same second at app launch is the normal case, not an edge
case, and destructive table-recreation migrations running twice is how databases
get destroyed. `tests/test_schema_migration_concurrency.py` proves single-apply
with real processes rather than threads.

The app this came from had 27 migrations behind that runner. The template keeps
one, because the runner is the part worth copying.

### `packaging/`

[docs/nuitka-lessons.md](docs/nuitka-lessons.md) covers the build flags:
`--nofollow-import-to` and the 14 GB to 2 GB memory result, the lazy-import trap
and its licensing consequence, why `no_annotations` is not free size savings,
the `externalBin` target-triple rename, and two flags that crash the build.

`generate_latest_json.py` writes the Tauri updater manifest and exists because
this step has three silent failure modes: the git tag must match the URL in the
manifest character for character, the signature must come from *this* build, and
the version must match `tauri.conf.json`. Each one produces an update that
simply never arrives, with no error anywhere.

### `src-tauri/nsis/hooks.nsh`

A `taskkill /T` plus a `wmic` command-line match, because a process spawned with
`CREATE_NEW_PROCESS_GROUP` escapes the tree kill. The `$UpdateMode` guards are
what stop an update from deleting the user's data on the way past.

### `src-tauri/.cargo/`

A cargo test runner for a Windows failure this template does not itself hit, kept
as a working reference because the fix is hard to find. In an app whose lib
unit-test binary reaches `tauri-plugin-dialog`'s dialog code, `cargo test` dies
in the loader with `STATUS_ENTRYPOINT_NOT_FOUND` before a single test runs. This
crate's test binary imports nothing from comctl32, so plain `cargo test` works.

The runner is registered in `test-runner.toml`, not `config.toml`, and is opted
into per command:

```powershell
cargo test --config .cargo\test-runner.toml
```

That split matters. Cargo applies `runner` to `cargo run` as well as `cargo
test`, so registering it in `config.toml` makes `tauri dev` launch the app
through `powershell.exe`; when the parent pipes cargo's stdio, Windows allocates
a console for it and an empty terminal window appears beside the app.
`-WindowStyle Hidden` does not suppress it when Windows Terminal is the default
console host. [docs/comctl32-test-runner.md](docs/comctl32-test-runner.md)
explains the whole thing, and is written to stand alone.

---

## 6. Provenance and maintenance

This is extracted from a discontinued commercial desktop application - a Tauri
v2 app with a Python sidecar, built and shipped for Windows. The supervision,
transport, migration, packaging and update code ran in production. It is
published because the pattern was worth more than the product, and because
nothing equivalent is published anywhere.

What is original code from that application, carried across with its comments:
the TCP client, the supervisor, the path resolution, the Python server and its
handshake, the PID lock, the shutdown flag, the migration runner and its lock,
the Nuitka build script, the signing script, the NSIS hooks, the release
workflow and the comctl32 runner.

What was written fresh for the template: the configuration dataclass, the three
demo handlers, the single demo migration, the `latest.json` generator, the Tauri
`run()` function and the demo UI. Roughly 10,400 lines of application-specific
handlers were discarded, along with 27 migrations, the hotkey, tray, clipboard
and notification subsystems, and everything else that was the product rather
than the pattern.

**No maintenance is promised.** This is a published reference, not a supported
project. There is no roadmap, issues may go unanswered, and it may never see
another commit. Fork it, copy the pieces you need, and treat it as a snapshot of
something that worked rather than as a dependency.

Corrections are welcome, particularly to the comctl32 note - if that explanation
saves someone the afternoon it cost to find, it has paid for the whole
repository.

MIT. See [LICENSE](LICENSE).
