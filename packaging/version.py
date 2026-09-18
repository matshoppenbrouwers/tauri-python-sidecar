"""Single source of truth for the version across every component.

Six files in this template carry a version string — Python, Rust, Tauri and two
package.json files. Letting them drift is not cosmetic: the Tauri updater
compares `tauri.conf.json`'s version against the one in `latest.json`, and the
release tag has to match the download URL exactly, so one stale file turns into
a silent 404 for every user on auto-update.

This file is read by:
- packaging/sync-versions.py  — writes this version into all the others
- packaging/nuitka-build.py   — stamps it into the executable's version resource
- packaging/generate_latest_json.py — builds the updater manifest
- CI                          — `sync-versions.py --check` gates the release
"""

VERSION = "0.1.0"
CHANNEL = "stable"  # stable, beta, nightly

# Semantic version components, for tooling that needs them separately.
VERSION_MAJOR = 0
VERSION_MINOR = 1
VERSION_PATCH = 0
