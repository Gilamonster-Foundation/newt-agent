# Persistent Shell Context Plugin Design

## Problem Statement

When newt-agent runs shell commands, each `run_command` call executes in a fresh subshell. This means:
- `cd /some/dir; export FOO=bar` doesn't persist to subsequent commands
- The agent has no concept of "current working directory" or environment state across turns
- Workflows that require sequential shell operations (build, test, deploy) lose context

## Design Goals

1. **Persist CWD**: `cd /path` should make `/path` the working directory for all subsequent commands
2. **Persist env vars**: `export FOO=bar` should persist across commands
3. **Minimal overhead**: No significant latency addition to command execution
4. **Transparent**: The agent shouldn't need special syntax — just use normal shell commands

## Architecture

### Plugin Structure

```
plugins/
  persistent-shell/
    plugin.py          # Plugin registration and lifecycle
    context_manager.py # Core state management
    hooks.py           # Command interception logic
    tests/             # Unit tests
```

### State Storage

Use a simple JSON file at `~/.newt/shell-context.json`:

```json
{
  "cwd": "/home/hartsock/workspaces/project",
  "env_vars": {
    "FOO": "bar",
    "PYTHONPATH": "/some/path"
  },
  "updated_at": "2026-07-09T19:30:00Z"
}
```

### Command Interception Logic

The plugin hooks into `run_command` and:
1. **Before execution**: Load current context, prepend `cd <cwd>` and relevant env exports to the command
2. **After execution**: Parse output and environment changes (for `cd`, `export`, etc.) and update state file
3. **On startup**: Check if state file exists; if so, load it into the session

### Interception Implementation

Use a wrapper around subprocess that:
- Reads context from state file before each command
- Prepends shell commands to restore CWD and env vars
- Executes the user's actual command
- Parses the resulting environment (via `env` output) for changes
- Updates state file if changes detected

Example intercepted command:
```bash
# User runs: run_command("cargo build")
# Plugin intercepts as:
cd /home/hartsock/workspaces/project && export FOO=bar && cargo build
```

### Parsing Environment Changes

After each command, capture `env` output and diff against stored state. For simple cases (most common), this is sufficient. Edge cases like `unset`, multi-line exports, or complex shell syntax will be documented as limitations.

## Limitations & Known Issues

1. **No support for `source`**: Sourcing files modifies the current shell; our approach can't capture these changes
2. **Complex shell syntax**: `export VAR=$(cmd)` may not persist correctly if `cmd` relies on prior state
3. **No undo**: Once a change is persisted, there's no way to "revert" without manual intervention
4. **Race conditions**: If multiple agents run simultaneously, they could overwrite each other's context (mitigated by using per-user state file)

## Testing Strategy

1. **Unit tests** for `context_manager.py`:
   - Load/save state correctly
   - Handle missing state file gracefully
   - Parse env var changes accurately

2. **Integration tests**:
   - `cd` persists across commands
   - `export` persists across commands
   - Mixed operations work correctly

3. **Edge case tests**:
   - Empty state file
   - Invalid JSON in state file (corruption recovery)
   - Concurrent access (if applicable)

## Future Considerations

- **Plugin configuration**: Allow users to specify which env vars to persist vs. ignore
- **Context profiles**: Save/restore named contexts (e.g., "dev", "prod")
- **Git integration**: Automatically set CWD based on git repo root
- **Tmux/screen support**: Integrate with terminal multiplexers for true persistent shells

## Implementation Priority

1. Core state management (`context_manager.py`)
2. Command interception logic (`hooks.py`)
3. Plugin registration and lifecycle (`plugin.py`)
4. Tests (unit + integration)
5. Documentation
