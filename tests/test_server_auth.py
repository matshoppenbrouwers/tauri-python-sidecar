"""Tests for the sidecar TCP server's per-session token authentication."""

from __future__ import annotations

import asyncio
import json
import platform
from unittest.mock import AsyncMock, MagicMock

import pytest

from sidecar.server import SidecarTcpServer


@pytest.fixture
def temp_index_dir(tmp_path):
    """Create a temporary index directory for the token file."""
    index_dir = tmp_path / ".index"
    index_dir.mkdir()
    return index_dir


@pytest.fixture
def server(temp_index_dir):
    """Create a SidecarTcpServer without touching real config/paths."""
    srv = SidecarTcpServer.__new__(SidecarTcpServer)
    srv.host = "127.0.0.1"
    srv.port = 0
    srv._server = None
    srv._connections = set()
    srv._require_auth = True
    srv._token = None
    srv._token_file = temp_index_dir / "sidecar_server.token"
    return srv


class TestTokenGeneration:
    def test_generate_token_creates_file(self, server):
        server._generate_session_token()

        assert server._token is not None
        assert len(server._token) > 20  # token_urlsafe(32) → ~43 chars
        assert server._token_file.exists()
        assert server._token_file.read_text() == server._token

    def test_generate_token_restricted_permissions_unix(self, server):
        if platform.system() == "Windows":
            pytest.skip("Unix permissions not applicable on Windows")

        server._generate_session_token()

        assert server._token_file.stat().st_mode & 0o777 == 0o600

    def test_cleanup_token_deletes_file(self, server):
        server._generate_session_token()
        assert server._token_file.exists()

        server._cleanup_session_token()

        assert not server._token_file.exists()
        assert server._token is None

    def test_cleanup_token_no_file_no_error(self, server):
        server._token = None
        server._cleanup_session_token()  # should not raise


def _mock_streams(first_line: str | bytes | None, *, timeout: bool = False):
    """Build mock (reader, writer) where reader.readline yields one line."""
    reader = MagicMock()
    if timeout:
        reader.readline = AsyncMock(side_effect=asyncio.TimeoutError)
    else:
        if first_line is None:
            payload = b""
        elif isinstance(first_line, bytes):
            payload = first_line
        else:
            payload = (first_line + "\n").encode("utf-8")
        reader.readline = AsyncMock(return_value=payload)

    writer = MagicMock()
    writer.write = MagicMock()
    writer.drain = AsyncMock()
    return reader, writer


class TestAuthenticate:
    @pytest.mark.asyncio
    async def test_correct_token_accepted_silently(self, server):
        server._generate_session_token()
        reader, writer = _mock_streams(json.dumps({"type": "auth", "token": server._token}))

        assert await server._authenticate(reader, writer) is True
        # No ack is written on success (preserves one-response-line contract)
        writer.write.assert_not_called()

    @pytest.mark.asyncio
    async def test_wrong_token_rejected(self, server):
        server._generate_session_token()
        reader, writer = _mock_streams(json.dumps({"type": "auth", "token": "nope"}))

        assert await server._authenticate(reader, writer) is False
        writer.write.assert_called_once()
        sent = json.loads(writer.write.call_args[0][0].decode("utf-8"))
        assert sent["error"]["message"] == "Unauthorized"

    @pytest.mark.asyncio
    async def test_non_auth_message_rejected(self, server):
        server._generate_session_token()
        reader, writer = _mock_streams(
            json.dumps({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": {}})
        )

        assert await server._authenticate(reader, writer) is False

    @pytest.mark.asyncio
    async def test_invalid_json_rejected(self, server):
        server._generate_session_token()
        reader, writer = _mock_streams("not valid json")

        assert await server._authenticate(reader, writer) is False

    @pytest.mark.asyncio
    async def test_empty_line_rejected(self, server):
        server._generate_session_token()
        reader, writer = _mock_streams(None)

        assert await server._authenticate(reader, writer) is False

    @pytest.mark.asyncio
    async def test_timeout_rejected(self, server):
        server._generate_session_token()
        reader, writer = _mock_streams("", timeout=True)

        assert await server._authenticate(reader, writer) is False

    @pytest.mark.asyncio
    async def test_no_token_set_rejects_everything(self, server):
        server._token = None
        reader, writer = _mock_streams(json.dumps({"type": "auth", "token": ""}))

        assert await server._authenticate(reader, writer) is False
