#!/usr/bin/env python
"""Create the placeholder `externalBin` file that `tauri dev` insists on.

`tauri.conf.json` declares `bundle.externalBin: ["binaries/py-sidecar"]`, which
is what makes the installer ship the compiled sidecar next to the app binary.
Tauri resolves that entry to one exact filename —
`binaries/py-sidecar-<target triple>.exe` — and its build script fails with
`resource path ... doesn't exist` when the file is missing. That check runs in
development too, so a fresh clone cannot even start `tauri dev` until something
has produced a 7 MB Nuitka binary, which takes minutes and needs MSVC.

The file does not have to be the real sidecar in development, and an empty one
is enough: `paths::get_sidecar_config()` branches on `debug_assertions` and runs
`.venv/Scripts/python.exe -m sidecar.loader` instead, so the placeholder is
copied around by the Tauri CLI and never executed.

This is deliberately NOT wired into `pnpm tauri build`. A release build that
quietly substituted an empty placeholder for a sidecar that failed to build
would produce an installer that installs a 0-byte `py-sidecar.exe` and fails at
first launch, which is exactly the class of silent packaging bug the rest of
this template is written to avoid. Run `packaging/nuitka-build.py` for that.

Usage:
    python packaging/dev_placeholder.py
"""

from __future__ import annotations

import sys
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
TARGET_TRIPLE = "x86_64-pc-windows-msvc"
PLACEHOLDER = PROJECT_ROOT / "src-tauri" / "binaries" / f"py-sidecar-{TARGET_TRIPLE}.exe"


def main() -> int:
    if PLACEHOLDER.exists():
        return 0

    PLACEHOLDER.parent.mkdir(parents=True, exist_ok=True)
    PLACEHOLDER.touch()
    print(f"Created development placeholder: {PLACEHOLDER.name}")
    print("  Empty on purpose - dev mode runs the sidecar from source.")
    print("  Run packaging/nuitka-build.py to replace it with the real binary.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
