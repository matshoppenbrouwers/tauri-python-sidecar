# The auth handshake

The sidecar listens on a TCP socket. Anything else running as the same user - 
a browser page via a fetch to `127.0.0.1`, another installed application, a
script - can open that socket. Binding to loopback keeps it off the network; it
does not keep it away from the rest of the machine. The handshake is the second
layer.

## The contract

On startup the server generates a token and writes it to a file:

```python
self._token = secrets.token_urlsafe(32)
self._token_file.unlink(missing_ok=True)
fd = os.open(self._token_file, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
try:
    os.write(fd, self._token.encode("utf-8"))
finally:
    os.close(fd)
```

The file is **created** restricted rather than written and then `chmod`ed. The
obvious two-step version leaves the token on disk readable at the process umask
between the write and the `chmod`, which is the window an attacker on a shared
machine needs. `O_EXCL` after the unlink also refuses to follow a symlink
planted at that path.

The token file sits in the same `.index/` directory as the database and the PID
lock, next to the socket it protects.

A client's **first line** on a new connection must be:

```json
{"type": "auth", "token": "<the token from the file>"}
```

On success the server says nothing and moves straight to reading JSON-RPC
requests. On failure it writes one JSON-RPC error and closes:

```json
{"jsonrpc": "2.0", "id": null, "error": {"code": -32000, "message": "Unauthorized"}}
```

Implementation: `sidecar/server.py`, `_generate_session_token` /
`_authenticate` / `_cleanup_session_token`. The Rust side is
`write_auth_handshake` in `src-tauri/src/sidecar_client.rs`.

## Four decisions worth copying

**Silence on success.** The protocol is one response line per request. If the
server acknowledged the handshake, the first request of a connection would get
two lines back and every client would need a special case for it. Staying silent
keeps the contract uniform.

**`hmac.compare_digest`, not `==`.** String equality on a secret returns as soon
as two bytes differ, so the time it takes leaks how much of the prefix was
right. `compare_digest` takes the same time regardless. Over loopback the signal
is noisy, but the correct comparison costs nothing.

**A per-session token, not a fixed one.** It is generated at server start and
deleted at shutdown. A token captured from a crashed session is useless against
the next one, and there is no secret to provision, rotate or store.

**File permissions are the real access control.** The token is only as private
as the file. The `0o600` mode argument is a POSIX concept and Windows ignores it.
On Windows the protection is instead that the file lives under the user's own
`%APPDATA%`, which other users cannot read. That is weaker than mode
0600 and it is worth knowing: on a shared Windows machine with an administrator
you do not control, this handshake does not defend against that administrator.
Nothing available in a sidecar's threat model does.

## The client side

`read_sidecar_token()` returns `None` when the file is absent, and the client
then sends no handshake line at all. That is what makes the server's
`require_auth=False` mode usable for tests: one code path, two configurations.

The failure mode to recognise: if the Rust data directory and the Python
`SIDECAR_DATA_DIR` disagree, the client reads a token file that does not exist,
sends nothing, and the server rejects it. The symptom is `Unauthorized` on a
sidecar that is plainly running and healthy. The supervisor injects
`SIDECAR_DATA_DIR` at spawn precisely so those two cannot drift; see
`spawn_sidecar_server_process` in `src-tauri/src/lib.rs`.

## Testing it

`tests/test_server_auth.py` covers the handshake: a correct token proceeds, a
wrong token gets `Unauthorized`, a missing handshake gets `Unauthorized`.
