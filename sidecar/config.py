"""Sidecar runtime configuration.

Everything the sidecar process needs to know is injected by its supervisor as
environment variables at spawn time, because the supervisor is the side that
decides them: it allocates the TCP port dynamically (see `allocate_free_port`
in `src-tauri/src/paths.rs`) and it owns the platform data directory.

Reading them here, rather than from a config file, keeps the two processes from
disagreeing about which port or database the app is using.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

#: Default port. Only used when the supervisor did not inject one, i.e. when you
#: run `python -m sidecar.loader sidecar-server` by hand for debugging.
DEFAULT_PORT = 9124


@dataclass(frozen=True)
class SidecarConfig:
    """Resolved runtime configuration for one sidecar process."""

    host: str
    port: int
    data_dir: Path
    log_level: str

    @property
    def index_dir(self) -> Path:
        """Directory for the database, lock file, token file and logs.

        Mirrors `get_index_dir()` on the Rust side — both must resolve to the
        same place or the client will look for the session token where the
        server never wrote it.
        """
        return self.data_dir / ".index"

    @property
    def db_path(self) -> Path:
        return self.index_dir / "sidecar.db"


def load_config() -> SidecarConfig:
    """Build the configuration from the environment.

    Bind to loopback only. The token handshake in `server.py` is the second
    layer; binding to 127.0.0.1 is the first, and it is the one that keeps the
    socket off the local network entirely.
    """
    return SidecarConfig(
        host=os.environ.get("SIDECAR_HOST", "127.0.0.1"),
        port=int(os.environ.get("SIDECAR_PORT", DEFAULT_PORT)),
        data_dir=Path(os.environ.get("SIDECAR_DATA_DIR", ".")),
        log_level=os.environ.get("SIDECAR_LOG_LEVEL", "INFO"),
    )
