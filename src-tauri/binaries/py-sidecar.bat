@echo off
REM Development shim for the Python sidecar.
REM
REM Tauri resolves `externalBin` entries to a real file, so a checkout that has
REM not run packaging/nuitka-build.py yet has nothing to point at. This wrapper
REM stands in for the frozen binary: same arguments, same behaviour, running
REM from source instead.

REM This script lives in src-tauri/binaries, so the project root is two up.
set SCRIPT_DIR=%~dp0
cd /d "%SCRIPT_DIR%\..\.."

REM Pass every argument through: the mode and --worker-name both matter.
python -m sidecar.loader %*
