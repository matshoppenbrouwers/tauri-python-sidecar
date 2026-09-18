#!/usr/bin/env python
"""Nuitka build script for the Python sidecar.

Freezes the `sidecar` package into a single Windows executable using Nuitka
--onefile mode, which is the shape Tauri's `externalBin` feature expects.

Output:
    src-tauri/binaries/py-sidecar-x86_64-pc-windows-msvc.exe

Usage:
    python packaging/nuitka-build.py           # Build sidecar
    python packaging/nuitka-build.py --clean   # Clean and rebuild
    python packaging/nuitka-build.py --check   # Check dependencies only

Requirements:
    pip install -r packaging/build-requirements.txt

Note: Uses --onefile mode (single self-extracting executable):
    - ~1-3s cold start due to onefile extraction to a temp directory
    - Required for Tauri externalBin (it expects a single flat .exe)
    - Subsequent runs are faster (cached extraction)
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

# Import version info
PACKAGING_DIR = Path(__file__).parent
PROJECT_ROOT = PACKAGING_DIR.parent
sys.path.insert(0, str(PACKAGING_DIR))

from version import VERSION  # noqa: E402

# Build configuration
OUTPUT_DIR = PROJECT_ROOT / "src-tauri" / "binaries"
OUTPUT_NAME = "py-sidecar"
# Tauri expects this naming convention for external binaries
TARGET_TRIPLE = "x86_64-pc-windows-msvc"

# Packages to compile into the binary.
#
# Nuitka's auto-discovery follows static imports from the entry point, which
# covers most of a well-behaved package. It does NOT reliably follow imports
# that only happen at runtime — a plugin registry, a factory that imports by
# name, a dependency that imports its own backends dynamically. Naming a package
# here forces it in regardless of how it is reached.
#
# `filelock` is the example in this template: `storage/migration_lock.py`
# imports it normally, but if it were ever imported lazily the frozen binary
# would build clean and then fail the first time two processes raced on a
# migration. Deterministic bundling is worth the one line.
INCLUDE_PACKAGES = [
    "sidecar",
    "sidecar.storage",
    "filelock",
]

# Packages Nuitka must NOT follow, even if something imports them.
#
# THE MEMORY LESSON: in the application this template was extracted from, the
# build peaked at ~14 GB of RAM and routinely died on a 16 GB machine. The cause
# was Nuitka following imports into heavy ML libraries (torch, transformers,
# numpy, sklearn). Adding --nofollow-import-to for those took peak build memory
# from 14 GB to roughly 2 GB. Nothing else came close to that effect.
#
# THE LAZY-IMPORT TRAP: Nuitka follows function-level imports too. A guarded
#
#     def extract(path):
#         import somebigdependency          # "only loaded if the user needs it"
#
# still drags the whole dependency into the binary — along with its licence
# obligations. Runtime laziness is not build-time exclusion; only
# --nofollow-import-to is. See docs/nuitka-lessons.md.
#
# The names below are not dependencies of this template. They are kept as a
# working example of the flag that mattered, and they cost nothing: a
# --nofollow-import-to for a package that is never imported is a no-op.
NOFOLLOW_IMPORTS = [
    # Test and dev tooling that has no business in a shipped binary
    "pytest",
    "_pytest",
    "unittest",
    "coverage",
    "pip",
    "setuptools",
    "wheel",
    "tkinter",
    # Heavy libraries — the 14 GB -> 2 GB win
    "torch",
    "transformers",
    "sentence_transformers",
    "numpy",
    "sklearn",
    "accelerate",
]


def check_nuitka_installed() -> bool:
    """Check if Nuitka and dependencies are installed."""
    try:
        import nuitka  # noqa: F401

        return True
    except ImportError:
        return False


def clean_build() -> None:
    """Remove previous build artifacts."""
    # Remove onefile exe
    exe_file = OUTPUT_DIR / f"{OUTPUT_NAME}-{TARGET_TRIPLE}.exe"
    if exe_file.exists():
        print(f"Removing previous build: {exe_file}")
        exe_file.unlink()

    # Remove old standalone directory (if any from previous builds)
    build_dir = OUTPUT_DIR / f"{OUTPUT_NAME}-{TARGET_TRIPLE}"
    if build_dir.exists():
        print(f"Removing previous standalone dir: {build_dir}")
        shutil.rmtree(build_dir)

    # Nuitka names its intermediate directories after the entry-point module,
    # so these are loader.* — and they land under --output-dir, NOT under the
    # working directory the build ran from. --remove-output usually deletes them
    # itself; they survive an interrupted or failed build, which is exactly when
    # --clean gets used.
    for name in ("loader.build", "loader.onefile-build", "loader.dist"):
        stale = OUTPUT_DIR / name
        if stale.exists():
            print(f"Removing Nuitka intermediate: {stale}")
            shutil.rmtree(stale)


def build_sidecar() -> int:
    """Build the sidecar executable with Nuitka."""
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)

    nuitka_args = [
        sys.executable,
        "-m",
        "nuitka",
        # Entry point - the unified loader
        str(PROJECT_ROOT / "sidecar" / "loader.py"),
        # Output settings
        f"--output-dir={OUTPUT_DIR}",
        f"--output-filename={OUTPUT_NAME}.exe",
        # Onefile mode (single executable) - required for Tauri externalBin
        # Note: has ~1-3s startup overhead on first run (extracts to temp)
        "--onefile",
        # Windows-specific. Console disabled because the sidecar is spawned by
        # the Tauri app: a console subsystem binary flashes a black window on
        # every start and every supervisor restart. server.py redirects
        # stdout/stderr to a log file precisely because this leaves it with none.
        "--windows-console-mode=disable",
        f"--windows-icon-from-ico={PROJECT_ROOT / 'src-tauri' / 'icons' / 'icon.ico'}",
    ]

    nuitka_args += [f"--include-package={pkg}" for pkg in INCLUDE_PACKAGES]
    nuitka_args += [f"--nofollow-import-to={pkg}" for pkg in NOFOLLOW_IMPORTS]

    nuitka_args.extend([
        # Optimization
        "--lto=yes",  # Link-time optimization
        "--python-flag=no_docstrings",  # Strip docstrings
        "--python-flag=no_asserts",  # Strip asserts in release
        # NOTE: DO NOT add --python-flag=no_annotations - it breaks Pydantic v2.
        # Pydantic builds its validators by reading __annotations__ at import
        # time, so stripping annotations produces a binary that raises on model
        # construction rather than at build time. The same applies to any
        # library that does runtime introspection of type hints: attrs,
        # dataclasses with resolved hints, FastAPI. This template does not use
        # Pydantic, but the flag looks like free size savings and is not.
        "--remove-output",  # Clean intermediate files
        # Version resource for the signed executable
        "--company-name=TODO_YOUR_NAME",
        "--product-name=Python Sidecar",
        f"--file-version={VERSION}",
        f"--product-version={VERSION}",
        "--file-description=Supervised Python sidecar service",
        "--copyright=TODO_YOUR_COPYRIGHT",
        # Build progress reporting - DELIBERATELY DISABLED.
        # Both of these crash Nuitka inside reportMemoryUsage() on Windows,
        # taking down an otherwise healthy build minutes in. It is undocumented
        # upstream; the failure looks like a build error in your own code.
        # If you enable them to debug a build, expect to turn them off again.
        # "--show-progress",
        # "--show-memory",
    ])

    # Tauri expects: binaries/py-sidecar-x86_64-pc-windows-msvc.exe
    final_exe = OUTPUT_DIR / f"{OUTPUT_NAME}-{TARGET_TRIPLE}.exe"

    print("Building sidecar with Nuitka...")
    print("  Entry point: sidecar/loader.py")
    print(f"  Output: {final_exe}")
    print(f"  Version: {VERSION}")
    print()

    try:
        subprocess.run(nuitka_args, check=True, cwd=PROJECT_ROOT)

        # Onefile mode writes exactly the filename we asked for. Nuitka has no
        # option to emit the target-triple name directly, and Tauri resolves an
        # `externalBin` entry of "binaries/py-sidecar" to
        # "py-sidecar-<target triple>.exe" and nothing else — so the rename is
        # not cosmetic, it is what makes the bundle find the binary at all.
        nuitka_output = OUTPUT_DIR / f"{OUTPUT_NAME}.exe"
        if not nuitka_output.exists():
            print(f"ERROR: Nuitka output not found: {nuitka_output}")
            print("Build may have failed silently or output path changed.")
            return 1

        # Windows refuses to unlink a running image, and the previous build's
        # sidecar is very often still alive: the supervisor spawns it DETACHED,
        # so closing the app or the terminal does not take it down. Nuitka has
        # already spent its several minutes by this point, so failing here with
        # a bare PermissionError wastes the whole build. Say what to kill.
        if final_exe.exists():
            try:
                final_exe.unlink()
            except PermissionError:
                print(f"ERROR: Cannot replace {final_exe.name} - it is in use.")
                print("  A previous sidecar is still running. Stop it with:")
                print(f"    taskkill /IM {final_exe.name} /F")
                return 1
        nuitka_output.rename(final_exe)

        if not final_exe.exists():
            print(f"ERROR: Final executable not found: {final_exe}")
            print("Expected executable was not created. Check Nuitka output above.")
            return 1

        print()
        print(f"Build complete: {final_exe}")

        size_mb = final_exe.stat().st_size / (1024 * 1024)
        print(f"  Size: {size_mb:.1f} MB")

        # Sanity check: a onefile build of anything real is several MB. A
        # suspiciously small file usually means Nuitka compiled the entry point
        # and followed nothing.
        if size_mb < 1.0:
            print(f"WARNING: Executable seems too small ({size_mb:.1f} MB). Build may be incomplete.")

        return 0

    except subprocess.CalledProcessError as e:
        print(f"Nuitka build failed with exit code {e.returncode}")
        return e.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description="Build the Python sidecar with Nuitka")
    parser.add_argument("--clean", action="store_true", help="Clean previous build artifacts")
    parser.add_argument(
        "--check", action="store_true", help="Check build dependencies without building"
    )

    args = parser.parse_args()

    if not check_nuitka_installed():
        print("Error: Nuitka is not installed.")
        print("Install with: pip install -r packaging/build-requirements.txt")
        return 1

    if args.check:
        print("Nuitka is installed and ready.")
        return 0

    if args.clean:
        clean_build()
        print("Cleaned previous build artifacts.")

    return build_sidecar()


if __name__ == "__main__":
    sys.exit(main())
