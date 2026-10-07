# Windows ambient commands

When OCAP is explicitly disabled and a command's authority checks admit the
ambient route, Windows runs it in a supervised Brush child by default. Commands
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

The child prepends `tools/` beside the running executable to its PATH and
exports that PATH to descendants. The separately packaged command pack supplies
external utilities there; installing this route alone does not install tools.
The child inherits the operator's environment except Newt's stripped control
and credential variables. It reads no interactive input. Output uses the usual
live drain and command result, including exit status and timeout reporting.

Before script delivery, the parent assigns the child to a Windows kill-on-close
job. Assignment failure refuses execution. Timeout, cancellation and dropping
the supervisor close that job and terminate descendants. This ordinary ambient
entrypoint is separate from the authenticated confined worker.
