"""End-to-end JSON-RPC envelope tests over the three demo handlers."""

from __future__ import annotations

import json

import pytest

from sidecar.dispatcher import handle_jsonrpc_message
from sidecar.storage.schema import CURRENT_SCHEMA_VERSION


@pytest.fixture(autouse=True)
def isolated_data_dir(tmp_path, monkeypatch):
    """Point the sidecar at a throwaway data directory for every test."""
    monkeypatch.setenv("SIDECAR_DATA_DIR", str(tmp_path))
    return tmp_path


def _call(method: str, params: dict | None = None) -> dict:
    request = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}
    return json.loads(handle_jsonrpc_message(json.dumps(request)))


def test_ping_returns_pong():
    assert _call("ping")["result"]["pong"] is True


def test_echo_round_trips_message():
    assert _call("echo", {"message": "hello"})["result"]["message"] == "hello"


def test_echo_rejects_non_string():
    error = _call("echo", {"message": 42})["error"]
    assert error["code"] == -32602


def test_get_status_reports_applied_schema():
    result = _call("get_status")["result"]
    assert result["schema_version"] == CURRENT_SCHEMA_VERSION
    assert result["note_count"] == 0


def test_unknown_method_is_method_not_found():
    assert _call("no_such_method")["error"]["code"] == -32601


def test_malformed_json_is_parse_error():
    response = json.loads(handle_jsonrpc_message("not json"))
    assert response["error"]["code"] == -32700
    assert response["id"] == 0  # no id could be recovered


def test_missing_jsonrpc_field_is_invalid_request():
    response = json.loads(handle_jsonrpc_message(json.dumps({"id": 1, "method": "ping"})))
    assert response["error"]["code"] == -32600
