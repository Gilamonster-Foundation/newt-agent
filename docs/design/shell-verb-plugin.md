# Shell Verb Plugin Design for newt-agent

## Overview

This document proposes a **plugin-based shell verb system** for `newt-agent` that provides persistent state (CWD, environment variables) across sessions. The plugin registers shell verbs as native tools in the newt-agent tool registry (`modulex__tool_*`).

## Architecture

### Plugin Registration Model

The plugin registers each shell verb as a newt-agent tool using the existing `modulex__tool_search` / `modulex__tool_describe` / `modulex__tool_invoke` infrastructure:

```
~/.newt/plugins/shell-verb/
├── manifest.json          # Plugin metadata + tool definitions
├── verbs/
│   ├── cd.rs              # cd verb implementation
│   ├── pwd.rs             # pwd verb implementation  
│   ├── env.rs             # env verb implementation (read/set)
│   └── export.rs          # export verb implementation
```

### Tool Schema Mapping

Each shell verb becomes a newt-agent tool with this schema:

| Field | Example (cd) | Description |
|-------|--------------|-------------|
| `name` | `"shell_cd"` | Unique tool identifier |
| `description` | `"Change working directory"` | Human-readable description |
| `inputSchema` | `{ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }` | JSON Schema for parameters |
| `facet` | `"shell:verb"` | Tool category/facet |
| `mutates` | `true` | Indicates state mutation (for CWD) |

### Persistent State Storage

State is persisted in `~/.newt/state/shell/`:

```
~/.newt/state/shell/
├── cwd.json              # Current working directory: {"path": "/home/hartsock"}
├── env.json              # Environment variables snapshot: {"PATH": "...", "SHELL": "/bin/bash"}
└── history.json          # Command history for recent verbs
```

### Verb Implementations

#### `cd` (Change Directory)
- **Tool name**: `shell_cd`
- **Input**: `path` (string, required) - target directory path
- **Behavior**: 
  1. Resolve path relative to current CWD or as absolute
  2. Update `~/.newt/state/shell/cwd.json`
  3. Return success/failure with new CWD

#### `pwd` (Print Working Directory)
- **Tool name**: `shell_pwd`
- **Input**: none
- **Behavior**: Read and return current CWD from state file

#### `env` / `export`
- **Tool name**: `shell_env`, `shell_export`
- **Input**: `name=value` pairs (optional for env, required for export)
- **Behavior**: Read/write to `~/.newt/state/shell/env.json`

## Integration with newt-agent

### Discovery
The plugin directory is scanned at startup. Each `.rs` file in `verbs/` registers itself via the tool registry API.

### State Initialization
On first run, the plugin creates default state:
- CWD defaults to `$HOME`
- Env snapshot captures current shell environment

### Session Persistence
State persists across sessions (unlike ephemeral `run_command`). This enables:
- Agents maintaining context about "where they are" in a project
- Multi-step workflows that depend on directory state
- Cross-session environment variable preservation

## Example Usage

```rust
// Agent calls shell_cd tool
modulex__tool_invoke(
  name: "shell_cd",
  arguments: {"path": "/home/hartsock/workspaces/newt-agent"}
)

// Returns: {"status": "success", "cwd": "/home/hartsock/workspaces/newt-agent"}

// Agent calls shell_pwd tool  
modulex__tool_invoke(name: "shell_pwd")

// Returns: {"cwd": "/home/hartsock/workspaces/newt-agent"}
```

## Benefits Over `run_command`

1. **Persistent state** - CWD/env survives across sessions
2. **Structured output** - JSON responses instead of stdout parsing
3. **Tool registry integration** - Self-documenting via `modulex__tool_describe`
4. **Type safety** - Input validation via JSON Schema
5. **Plugin extensibility** - Easy to add new shell verbs

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| State corruption on crash | Atomic writes with `.tmp` suffix + rename |
| Security (env variable injection) | Validate env var names against `^[A-Za-z_][A-Za-z0-9_]*$` |
| Performance overhead | Minimal - state file is small JSON, read/write <1ms |

## Next Steps

1. Define exact manifest.json schema for plugin discovery
2. Implement core shell verb tools (cd, pwd, env)
3. Add state persistence layer with atomic writes
4. Write tests for CWD/env manipulation workflows
5. Document plugin development guide for new verbs
