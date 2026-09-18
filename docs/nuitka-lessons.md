# Nuitka lessons

Every published Tauri-plus-Python example compiles the sidecar with PyInstaller.
This template uses Nuitka, which compiles to C rather than bundling an
interpreter, and the differences show up in places the PyInstaller guides do not
cover. These are the things that cost real time to find.

The flags live in `packaging/nuitka-build.py`, each one commented where it is
used. This note is the longer version.

## `--nofollow-import-to` is the memory flag

In the application this template was extracted from, the Nuitka build peaked at
around **14 GB of RAM** and routinely died on a 16 GB machine - often twenty
minutes in, with an error that reads like a compiler bug rather than an
out-of-memory condition.

The cause was Nuitka following imports into heavy libraries: `torch`,
`transformers`, `sentence_transformers`, `numpy`, `sklearn`. Adding
`--nofollow-import-to` for each of them took peak build memory from roughly
14 GB to roughly 2 GB. Nothing else came close to that effect - not `--jobs`,
not `--lto`, not splitting the build.

The names are still in `NOFOLLOW_IMPORTS` in this template even though none of
them is a dependency here. A `--nofollow-import-to` for a package nothing
imports is a no-op, so keeping them costs nothing and keeps the flag that
mattered visible with a working example attached.

## Nuitka follows function-level lazy imports

This is the transferable lesson, and it is the one that surprises people.

```python
def extract_text(path):
    import somebigdependency          # "only loaded if the user needs it"
    ...
```

Deferring an import into a function makes it lazy **at runtime**. It does not
exclude it from the build. Nuitka's static analysis walks function bodies, finds
that import, and pulls the entire package - and its transitive dependencies - 
into the binary. The shipped executable contains a library that most users will
never trigger.

That matters for size, and it matters much more for licensing. Our own instance:
a guarded `import pymupdf` inside a PDF-handling function. PyMuPDF is AGPL-3.0.
It was in the binary regardless of whether any user ever opened a PDF, and the
AGPL obligations attached to the shipped artefact. The package was removed and
replaced for that reason.

**Two corrections to the folklore, because both halves of this story usually get
merged and the merged version is wrong:**

1. The 14 GB → 2 GB win came from `--nofollow-import-to` on the ML libraries. It
   had nothing to do with PyMuPDF.
2. PyMuPDF was dropped for AGPL-3.0 licensing, not for build memory.

What connects them is the mechanism: runtime laziness is not build-time
exclusion. Only `--nofollow-import-to` is. If you have a guarded import of
something whose licence you cannot accept in a shipped binary, auditing the
import statements is not enough - check the binary, and exclude the package
explicitly.

## `--python-flag=no_annotations` breaks runtime introspection

It looks like free size savings. It is not.

Any library that reads `__annotations__` at import time to build behaviour will
produce a binary that compiles clean and then raises at runtime. Pydantic v2 is
the common case: it constructs its validators from annotations, so stripping
them yields models that fail on construction rather than a build that fails
loudly. The same applies to `attrs`, to dataclasses with resolved hints, and to
FastAPI's dependency injection.

`--python-flag=no_docstrings` and `--python-flag=no_asserts` are safe by
comparison and are enabled here.

This template does not use Pydantic. The warning stays in the build script
because the flag is tempting, its failure is silent at build time, and the next
dependency you add may well read type hints.

## `--include-package` for anything imported dynamically

Nuitka bundles what it can see. A package reached by `importlib.import_module`,
by a plugin registry, or by a string name in configuration is invisible to
static analysis and simply will not be there.

`INCLUDE_PACKAGES` here names `sidecar`, `sidecar.storage` and `filelock`.
`filelock` is imported normally by `storage/migration_lock.py`, so it would be
followed anyway - it is named explicitly as the example of the habit. If it were
ever imported lazily, the binary would build clean and then fail the first time
two processes raced on a migration, in the field, on someone else's machine.
Deterministic bundling is worth one line per package.

## `--onefile` and the target-triple rename

Tauri's `externalBin` resolves `"binaries/py-sidecar"` to
`binaries/py-sidecar-<target triple>.exe` - on this platform,
`py-sidecar-x86_64-pc-windows-msvc.exe`. Not `py-sidecar.exe`, not any other
spelling. The build script renames Nuitka's output to exactly that name, and the
rename is functional rather than cosmetic: get it wrong and the bundler fails
with "resource path ... doesn't exist", in `tauri dev` as well as `tauri build`.

`--onefile` is what `externalBin` expects: a single self-extracting executable.
The cost is roughly 1–3 seconds of cold start on first run while it extracts to
a temporary directory. Each supervisor restart pays it again.

## `--windows-console-mode=disable`, and what it forces

A console-subsystem binary flashes a black window every time it starts - on
launch, and again on every supervisor restart. Disabling the console fixes that
and takes `stdout`/`stderr` away entirely: they become invalid handles, and
anything that writes to them can fail.

That is why `sidecar/server.py` redirects `stdout` and `stderr` to a log file
before doing anything else. The two settings are a pair. If you re-enable the
console, the redirect is what you are working against; if you disable it without
the redirect, you get a sidecar that dies at the first `print()` with no way to
find out why.

## `--show-progress` and `--show-memory` crash the build

Both crash Nuitka inside `reportMemoryUsage()` on Windows, taking down an
otherwise healthy build minutes in. The failure is undocumented upstream and
looks like an error in your own code.

They are present but commented out in the build script. If you enable one to
debug a build, expect to turn it off again.

## Practical notes

- Nuitka needs a C compiler. With the MSVC Rust toolchain installed, `cl` is
  already on `PATH` and Nuitka uses it rather than offering to download MinGW.
- Budget roughly four minutes for a cold build of this template. A real
  dependency tree takes considerably longer.
- The build script refuses to overwrite the output executable while the previous
  sidecar is still running, and names the `taskkill` command. This is a frequent
  case: the supervisor spawns the sidecar detached, so closing the app does not
  take it down.
- A onefile build of anything real is several megabytes. The script warns below
  1 MB, which usually means Nuitka compiled the entry point and followed
  nothing.
