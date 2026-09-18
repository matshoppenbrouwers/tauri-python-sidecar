#!/usr/bin/env python
"""Sync the version across every file in this template that carries one.

Usage:
    python packaging/sync-versions.py           # Sync to packaging/version.py
    python packaging/sync-versions.py --check   # Check consistency, change nothing
    python packaging/sync-versions.py 0.2.0     # Set a specific version and sync

`--check` returns a non-zero exit code when any file has drifted, which is what
makes it usable as a CI gate before a release build. Run it in the release
workflow BEFORE the expensive Nuitka and Tauri builds, not after: a version
mismatch discovered at the publish step has already cost you the build.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# Import version from sibling module
PACKAGING_DIR = Path(__file__).parent
PROJECT_ROOT = PACKAGING_DIR.parent
sys.path.insert(0, str(PACKAGING_DIR))

from version import VERSION  # noqa: E402


def update_file(path: Path, pattern: str, replacement: str, dry_run: bool = False) -> bool:
    """Update file content using regex substitution.

    Returns True if content was changed (or would be changed in dry_run).
    """
    if not path.exists():
        print(f"  [SKIP] {path.relative_to(PROJECT_ROOT)} - file not found")
        return False

    content = path.read_text(encoding="utf-8")
    new_content = re.sub(pattern, replacement, content)

    if content != new_content:
        if dry_run:
            print(f"  [DIFF] {path.relative_to(PROJECT_ROOT)} - would update")
        else:
            path.write_text(new_content, encoding="utf-8")
            print(f"  [OK] {path.relative_to(PROJECT_ROOT)}")
        return True

    print(f"  [OK] {path.relative_to(PROJECT_ROOT)} - already at target version")
    return False


def update_json_file(path: Path, key: str, version: str, dry_run: bool = False) -> bool:
    """Update a JSON file's version field.

    Returns True if content was changed (or would be changed in dry_run).
    """
    if not path.exists():
        print(f"  [SKIP] {path.relative_to(PROJECT_ROOT)} - file not found")
        return False

    content = path.read_text(encoding="utf-8")
    data = json.loads(content)

    current = data.get(key)
    if current == version:
        print(f"  [OK] {path.relative_to(PROJECT_ROOT)} - already at {version}")
        return False

    if dry_run:
        print(f"  [DIFF] {path.relative_to(PROJECT_ROOT)} - would update {current} -> {version}")
        return True

    data[key] = version
    new_content = json.dumps(data, indent=2) + "\n"
    path.write_text(new_content, encoding="utf-8")
    print(f"  [OK] {path.relative_to(PROJECT_ROOT)} - {current} -> {version}")
    return True


def sync_versions(version: str, dry_run: bool = False) -> int:
    """Sync version across all project files.

    Returns 0 if all files are consistent, 1 if changes were needed/made.
    """
    print(f"Syncing version to {version}...")
    if dry_run:
        print("(dry run - no files will be modified)")
    print()

    changed = False

    # Parse semantic version components
    parts = version.split("-")[0].split(".")
    major, minor, patch = parts[0], parts[1], parts[2]

    # packaging/version.py - VERSION and components
    version_py = PACKAGING_DIR / "version.py"
    if version_py.exists():
        content = version_py.read_text(encoding="utf-8")
        new_content = re.sub(r'VERSION = "[^"]+"', f'VERSION = "{version}"', content)
        new_content = re.sub(r"VERSION_MAJOR = \d+", f"VERSION_MAJOR = {major}", new_content)
        new_content = re.sub(r"VERSION_MINOR = \d+", f"VERSION_MINOR = {minor}", new_content)
        new_content = re.sub(r"VERSION_PATCH = \d+", f"VERSION_PATCH = {patch}", new_content)
        if content != new_content:
            if dry_run:
                print(f"  [DIFF] {version_py.relative_to(PROJECT_ROOT)} - would update")
            else:
                version_py.write_text(new_content, encoding="utf-8")
                print(f"  [OK] {version_py.relative_to(PROJECT_ROOT)}")
            changed = True
        else:
            print(f"  [OK] {version_py.relative_to(PROJECT_ROOT)} - already at target version")

    # pyproject.toml - anchored to line start so [tool.ruff] target-version and
    # friends are not rewritten along with it.
    pyproject = PROJECT_ROOT / "pyproject.toml"
    if pyproject.exists():
        content = pyproject.read_text(encoding="utf-8")
        new_content = re.sub(
            r'^version = "[^"]+"',
            f'version = "{version}"',
            content,
            count=1,
            flags=re.MULTILINE,
        )
        if content != new_content:
            if dry_run:
                print(f"  [DIFF] {pyproject.relative_to(PROJECT_ROOT)} - would update")
            else:
                pyproject.write_text(new_content, encoding="utf-8")
                print(f"  [OK] {pyproject.relative_to(PROJECT_ROOT)}")
            changed = True
        else:
            print(f"  [OK] {pyproject.relative_to(PROJECT_ROOT)} - already at target version")

    # src-tauri/Cargo.toml - the [package] version only, never a dependency's.
    cargo_toml = PROJECT_ROOT / "src-tauri" / "Cargo.toml"
    if cargo_toml.exists():
        content = cargo_toml.read_text(encoding="utf-8")
        # DOTALL lets .*? cross the lines between [package] and its version key;
        # count=1 stops the same pattern from reaching a dependency table below.
        new_content = re.sub(
            r'(\[package\].*?^version = )"[^"]+"',
            rf'\g<1>"{version}"',
            content,
            flags=re.MULTILINE | re.DOTALL,
            count=1,
        )
        if content != new_content:
            if dry_run:
                print(f"  [DIFF] {cargo_toml.relative_to(PROJECT_ROOT)} - would update")
            else:
                cargo_toml.write_text(new_content, encoding="utf-8")
                print(f"  [OK] {cargo_toml.relative_to(PROJECT_ROOT)}")
            changed = True
        else:
            print(f"  [OK] {cargo_toml.relative_to(PROJECT_ROOT)} - already at target version")

    # src-tauri/tauri.conf.json - this is the one the updater compares against
    # the version in latest.json. If it drifts, the app either never updates or
    # updates in a loop.
    changed |= update_json_file(
        PROJECT_ROOT / "src-tauri" / "tauri.conf.json",
        "version",
        version,
        dry_run,
    )

    # root package.json
    changed |= update_json_file(PROJECT_ROOT / "package.json", "version", version, dry_run)

    # ui/package.json
    changed |= update_json_file(PROJECT_ROOT / "ui" / "package.json", "version", version, dry_run)

    print()
    if changed:
        if dry_run:
            print(f"Version drift detected. Run without --check to sync to v{version}.")
            return 1
        print(f"All components synced to v{version}")
    else:
        print(f"All components already at v{version}")

    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Sync version across all project configuration files."
    )
    parser.add_argument(
        "version",
        nargs="?",
        default=VERSION,
        help=f"Version to sync (default: {VERSION} from packaging/version.py)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Check version consistency without modifying files",
    )

    args = parser.parse_args()

    if not re.match(r"^\d+\.\d+\.\d+(-[\w.]+)?$", args.version):
        print(f"Error: Invalid version format: {args.version}")
        print("Expected format: MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-prerelease")
        return 1

    return sync_versions(args.version, dry_run=args.check)


if __name__ == "__main__":
    sys.exit(main())
