#!/usr/bin/env python
"""Emit the `latest.json` manifest the Tauri updater polls.

Usage:
    python packaging/generate_latest_json.py            # write next to the installer
    python packaging/generate_latest_json.py --dry-run  # print it, using a dummy signature

The updater fetches this file, compares `version` against the running app's
version, then downloads `url` and verifies it against `signature` using the
public key in `tauri.conf.json`. Three ways that goes wrong, all of them silent:

1. `url` must point at an asset that exists. The release tag is part of the URL,
   so a tag of `v0.1.0-beta` against a URL containing `v0.1.0` is a 404 for
   every user. Tag and URL must match character for character.
2. `signature` is the entire contents of the `.exe.sig` file Tauri produced for
   THIS build. A signature from a previous build fails verification and the
   update is silently refused.
3. `version` must match `tauri.conf.json` — run `sync-versions.py --check`.

REPO_URL below is a placeholder. Point it at your own releases before shipping.
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

PACKAGING_DIR = Path(__file__).parent
PROJECT_ROOT = PACKAGING_DIR.parent
sys.path.insert(0, str(PACKAGING_DIR))

from version import VERSION  # noqa: E402

REPO_URL = "https://github.com/TODO_YOUR_ACCOUNT/TODO_YOUR_REPO"
BUNDLE_DIR = PROJECT_ROOT / "src-tauri" / "target" / "release" / "bundle" / "nsis"


def installer_name(product: str, version: str) -> str:
    return f"{product}_{version}_x64-setup.exe"


def build_manifest(product: str, version: str, signature: str) -> dict:
    installer = installer_name(product, version)
    return {
        "version": version,
        "notes": "See the release notes on GitHub",
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": {
            "windows-x86_64": {
                "signature": signature,
                "url": f"{REPO_URL}/releases/download/v{version}/{installer}",
            }
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate the Tauri updater manifest.")
    parser.add_argument("--version", default=VERSION, help=f"Version (default: {VERSION})")
    parser.add_argument("--product", default="tauri-python-sidecar", help="Bundle product name")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the manifest with a dummy signature instead of writing it",
    )
    args = parser.parse_args()

    if args.dry_run:
        print(json.dumps(build_manifest(args.product, args.version, "DUMMY_SIGNATURE"), indent=2))
        return 0

    sig_file = BUNDLE_DIR / f"{installer_name(args.product, args.version)}.sig"
    if not sig_file.exists():
        print(f"Error: signature not found: {sig_file}")
        print("Rebuild with TAURI_SIGNING_PRIVATE_KEY set; without it Tauri emits no .sig.")
        return 1

    out = BUNDLE_DIR / "latest.json"
    manifest = build_manifest(args.product, args.version, sig_file.read_text(encoding="utf-8").strip())
    out.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote {out}")
    print(f"  Release tag must be exactly: v{args.version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
