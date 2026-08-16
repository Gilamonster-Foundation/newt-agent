---
name: jupyter-server
description: "Start, stop, and monitor Jupyter notebook servers. Manage server lifecycle, query running kernels, and get server status via REST API."
version: 1.0.0
license: Apache-2.0
when_to_use: Need to launch a Jupyter server for interactive notebook development, manage multiple server instances, or query running kernels and their states. Useful for setting up development environments, running notebooks interactively in a browser, or monitoring server health.
caveats:
  exec:
    only:
      - "jupyter"
      - "kill"
      - "taskkill"
  fs_read:
    only: []
  fs_write:
    only: []
  net:
    only:
      - "localhost:*"
      - "127.0.0.1:*"
  max_calls: { at_most: 20 }
---

# Jupyter Server Management Skill

This skill provides the ability to start, stop, and monitor Jupyter notebook servers. It manages the server lifecycle, queries running kernels via the Jupyter REST API, and provides status information.

## Prerequisites

- **Jupyter** must be installed and available in PATH (`jupyter` command)
- **Python** with `jupyter_server` or `notebook` package installed
- Network access to localhost for API queries

## Core Tools

| Function | Description |
|----------|-------------|
| `start_jupyter_server` | Launch a new Jupyter server in the background |
| `stop_jupyter_server` | Stop a Jupyter server by PID |
| `get_jupyter_server_status` | Query server status and list running kernels |

## Data Types

### JupyterServerParams

Parameters for starting a Jupyter server:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `working_dir` | string | current directory | Working directory for the server |
| `port` | integer | 8888 | Port to run the server on |
| `host` | string | "localhost" | Host to bind to |
| `token` | string | auto-generated | Authentication token |
| `password` | string | none | Password for authentication (alternative to token) |
| `open_browser` | boolean | false | Whether to open browser on start |
| `extra_args` | list[string] | none | Additional command line arguments |

### JupyterServerResult

Result of starting a Jupyter server:

| Field | Type | Description |
|-------|------|-------------|
| `success` | bool | Whether server started successfully |
| `url` | string | Server URL (e.g., http://localhost:8888) |
| `pid` | integer | Server process ID |
| `port` | integer | Port the server is running on |
| `token` | string | Authentication token |
| `error` | string | Error message if failed |

### JupyterServerStatus

Status of a Jupyter server:

| Field | Type | Description |
|-------|------|-------------|
| `running` | bool | Whether server is running |
| `url` | string | Server URL |
| `pid` | integer | Process ID (if available) |
| `port` | integer | Port number (if available) |
| `kernels` | list[KernelInfo] | List of running kernels |

### KernelInfo

Information about a running kernel:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Kernel ID |
| `name` | string | Kernel name (e.g., "python3") |
| `last_activity` | string | ISO timestamp of last activity |
| `execution_state` | string | "idle", "busy", or "starting" |
| `connections` | integer | Number of active connections |

## Example Usage

### Start a Jupyter Server

```python
from newt_agent.tools import start_jupyter_server, JupyterServerParams

# Start server with defaults
params = JupyterServerParams(
    working_dir="/path/to/project",
    port=8888,
    open_browser=False
)
result = start_jupyter_server(params)

if result.success:
    print(f"Server started at {result.url}")
    print(f"Token: {result.token}")
    print(f"PID: {result.pid}")
else:
    print(f"Failed to start: {result.error}")
```

### Check Server Status

```python
from newt_agent.tools import get_jupyter_server_status

# Query status using URL and token from start result
status = get_jupyter_server_status(
    url="http://localhost:8888",
    token="your-token-here"
)

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

# Stop by PID (from start result)
stopped = stop_jupyter_server(pid=12345)
print(f"Stopped: {stopped}")
```

## Workflow

1. **Start** a server with `start_jupyter_server` — returns URL, token, and PID
2. **Use** the server — open URL in browser, run notebooks interactively
3. **Monitor** with `get_jupyter_server_status` — check kernels, activity
4. **Stop** with `stop_jupyter_server` — clean shutdown by PID

## Tips

- The server runs in the background; the Python process does not block
- Each `start_jupyter_server` call launches a new independent server
- Use different ports for multiple concurrent servers
- The token is auto-generated if not provided; save it for API access
- For password auth, set `password` and leave `token` empty
- `extra_args` can pass any `jupyter notebook` flags (e.g., `--NotebookApp.disable_check_xsrf=True`)

## Error Handling

Common errors:
- **Port in use**: Choose a different `port` or stop the existing server
- **Jupyter not installed**: Install with `pip install jupyter`
- **Permission denied**: Check directory permissions for `working_dir`
- **Server fails to start**: Check stderr in `error` field; may need kernel installation
- **Status query fails**: Server may have crashed; check if PID is still alive

## Security Notes

- Servers bind to `localhost` by default (not accessible externally)
- Token authentication is enabled by default
- For remote access, use SSH tunneling or configure `--ip=0.0.0.0` with proper auth
- Always stop servers when done to free ports and resources