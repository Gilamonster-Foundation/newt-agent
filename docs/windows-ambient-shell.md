# Windows ambient commands

When OCAP is explicitly disabled and a command's authority checks admit the
ambient route, Windows uses a supervised `tools/newt-ambient-brush.exe` child
by default. The path is relative to the embedding host executable, whether
that host is the CLI, a server, or another core consumer. It never re-executes
the host or searches PATH for the runner. If the runner is absent, commands
use `cmd.exe /C`, guidance names cmd syntax, and the operator sees
"carried brush not installed; using cmd" once per process. With Brush, commands
use POSIX syntax: pipelines, `;`, `$?`, redirection and quoted paths. Prefer
forward slashes or single quotes for Windows paths containing backslashes.

To restore `cmd.exe /C`, pass `--windows-cmd` or configure:

```toml
[shell]
windows_cmd = true
```

The flag overrides configuration. This preference selects syntax only; it does
not disable OCAP, bypass a broker or lease, or widen an exec floor. Confined
routes retain their existing engines and authenticated transport.

The parent prepends `tools/` beside the embedding host executable to the child PATH and
exports that PATH to descendants. The separately packaged command pack supplies
external utilities there; installing this route alone does not install tools.
The child inherits the operator's environment except Newt's stripped control
and credential variables. It reads no interactive input. Output uses the usual
live drain and command result, including exit status and timeout reporting.

Before script delivery, the parent assigns the child to a Windows kill-on-close
job. Assignment failure refuses execution. Timeout, cancellation and dropping
the supervisor close that job and terminate descendants. This ordinary ambient
runner is separate from the authenticated confined worker. The job-object
tree teardown applies to Brush; cmd retains its existing immediate-child
supervision.

## Installing the runner

The runner is a separate binary target of the `newt-agent` package. It links
the Brush interpreter directly, without re-entering CLI/server startup:

```powershell
cargo build --release --locked -p newt-agent --bin newt-ambient-brush
```

Stage the command pack using `scripts/windows/Install-CarriedTools.ps1` from
the carried-tools change first; that installer requires a fresh `tools/`.
Then copy `target/release/newt-ambient-brush.exe` into that SAME `tools/`
directory next to the host executable. Do not create a second directory or
install the runner before the fresh-directory pack installer. The removed
`newt __ambient-brush` entrypoint is not an installation fallback.
