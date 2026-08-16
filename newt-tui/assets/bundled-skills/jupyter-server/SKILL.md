---
name: jupyter-server
description: "Start, stop, and monitor Jupyter notebook servers. Manage server lifecycle by opaque handle, query running kernels, and get server status via the server's own REST API."
version: 1.1.0
license: Apache-2.0
when_to_use: Need to launch a Jupyter server for interactive notebook development, manage server instances, or query running kernels and their states. Useful for setting up development environments, running notebooks interactively in a browser, or monitoring server health.
caveats:
  exec:
    only:
      - "jupyter"
  fs_read:
    only: []
  fs_write:
    only: []
  net:
    only:
      - "127.0.0.1:*"
      - "localhost:*"
  max_calls: { at_most: 20 }
---

# Jupyter Server Management Skill

Start, stop, and monitor Jupyter notebook servers. Every server this tool
starts is owned by the calling process and tracked under an opaque **handle
id**; `stop` and `get_status` operate on that handle — never on a bare PID or
an arbitrary URL — so a caller cannot point this tool at a server it did not
start.

## Prerequisites

- **Jupyter** must be installed and on `PATH` (`jupyter` command)
- **Python** with `jupyter_server` or `notebook` package installed
- Network access to loopback (`127.0.0.1` / `localhost`) only

## Core Tools

| Function | Description |
|----------|-------------|
| `start_jupyter_server` | Launch a Jupyter server in the background; returns a `handle_id` |
| `stop_jupyter_server` | Stop a server by `handle_id` (kills the owned child directly — no `kill`/`taskkill` subprocess) |
| `get_jupyter_server_status` | Query a server's kernels by `handle_id` via its own REST API |

## Data Types

### JupyterServerParams

Parameters for starting a Jupyter server:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `working_dir` | string | current directory | Working directory for the server |
| `port` | integer | 8888 | Port to run the server on |
| `host` | string | `127.0.0.1` | Bind address — **must** be loopback (`127.0.0.1` / `::1` / `localhost`); non-loopback is rejected before spawn |
| `token` | string | auto-generated (32 random chars) | Authentication token |
| `password` | string | none | Password for authentication (alternative to token) |
| `open_browser` | boolean | false | Always forced off (`--no-browser`); the field is kept for API compatibility |
| `extra_args` | list[string] | none | Additional `jupyter notebook` flags (caller-controlled — use with care) |

### JupyterServerResult

Result of starting a Jupyter server:

| Field | Type | Description |
|-------|------|-------------|
| `success` | bool | Whether server started successfully |
| `handle_id` | integer or None | Opaque handle for `stop` / `get_status` (None on failure) |
| `url` | string | Server URL (e.g. `http://127.0.0.1:8888`) |
| `pid` | integer | Server process ID (informational; operations use `handle_id`) |
| `port` | integer | Port the server is running on |
| `token` | string | Authentication token |
| `error` | string | Error message if failed |

### JupyterServerStatus

Status of a Jupyter server:

| Field | Type | Description |
|-------|------|-------------|
| `running` | bool | Whether server is running |
| `handle_id` | integer | The handle this status refers to |
| `url` | string | Server URL |
| `port` | integer | Port number |
| `kernels` | list[KernelInfo] | List of running kernels |

### KernelInfo

Information about a running kernel:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Kernel ID |
| `name` | string | Kernel name (e.g. "python3") |
| `last_activity` | string | ISO timestamp of last activity |
| `execution_state` | string | "idle", "busy", or "starting" |
| `connections` | integer | Number of active connections |

## Example Usage

### Start a Jupyter Server

```python
from newt_agent.tools import start_jupyter_server, JupyterServerParams

params = JupyterServerParams(
    working_dir="/path/to/project",
    port=8888,
)
result = start_jupyter_server(params)

if result.success:
    print(f"Server started at {result.url}")
    print(f"handle_id: {result.handle_id}")
    print(f"Token: {result.token}")
else:
    print(f"Failed to start: {result.error}")
```

### Check Server Status

```python
from newt_agent.tools import get_jupyter_server_status

# Query by the handle returned from start — not by URL/token.
status = get_jupyter_server_status(result.handle_id)

if status.running:
    print(f"Server is running with {len(status.kernels)} kernels")
    for kernel in status.kernels:
        print(f"  Kernel {kernel.id}: {kernel.name} ({kernel.execution_state})")
else:
    print("Server is not running")
```

### Stop a Jupyter Server

```python
from newt_agent.tools import stop_jupyter_server

# Stop by handle (from start result). A second stop is a no-op (returns False).
stopped = stop_jupyter_server(result.handle_id)
print(f"Stopped: {stopped}")
```

## Workflow

1. **Start** a server with `start_jupyter_server` — returns `handle_id`, URL, and token
2. **Use** the server — open URL in a browser, run notebooks interactively
3. **Monitor** with `get_jupyter_server_status(handle_id)` — check kernels, activity
4. **Stop** with `stop_jupyter_server(handle_id)` — kills the owned child directly

## Tips

- The server runs detached in the background; `start` does not block
- Each `start_jupyter_server` call launches a new independent server with its own handle
- Use different ports for multiple concurrent servers
- The token is auto-generated if not provided
- For password auth, set `password` and leave `token` empty
- `extra_args` can pass any `jupyter notebook` flags (caller-controlled — review before use)

## Error Handling

Common errors:
- **Port in use**: choose a different `port` or stop the existing server first
- **Non-loopback host**: rejected before spawn — use `127.0.0.1` / `::1` / `localhost` only
- **Jupyter not installed**: install with `pip install jupyter`
- **Server fails to start**: the `error` field includes the captured stderr tail; readiness is confirmed by a REST probe (up to ~20s), not a fixed sleep
- **Unknown handle**: `get_status` reports `running=False`; `stop` returns `False` (no-op, not an error)

## Security Notes

- Servers bind to `127.0.0.1` by default and **only** loopback is accepted — the server is reachable solely from the operator's own machine
- No remote-access / `--allow-origin` flags are passed; loopback binding plus the default `False` keeps the server off the network
- Token authentication is enabled by default (auto-generated)
- The child environment is scrubbed (`env_clear` + a minimal allowlist) so no newt control-plane secrets or authority switches reach the notebook subprocess
- A server can only be stopped or queried via the handle this process issued — no operating on foreign servers by PID/URL
- For access from another machine, use an SSH tunnel to the loopback port rather than binding to a non-loopback address