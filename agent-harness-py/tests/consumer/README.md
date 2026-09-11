# Foreign Python consumer

This independent Cargo workspace composes the public registration seam in its
own PyO3 extension. It depends on neither the umbrella wheel nor
newt-core. The Python smoke runs against a compiled, imported extension and
grounds the kernel's admission and byte-verification tests in CPython.

From the repository root, run `just harness-python`. It uses the same commands
as the dedicated CI job. To run only the Python smoke from this directory:

```bash
python3 check_dependencies.py
uv venv .venv
VIRTUAL_ENV="$PWD/.venv" uv tool run maturin develop --locked
.venv/bin/python smoke.py
```

The smoke checks unit and event admission, exact request replay, verdict
accounting, complete tool-output retention, navigation selection, auxiliary
observation recording, restoration in a fresh Python process, configuration
drift, and refusal after stored bytes change. It also checks competing and stale
writers leave the checkpoint unchanged, releases sessions before restoration,
and verifies inherited-session refusal through `os.fork()` where available.
The successful parent append grounds continued ownership after the child exits.
The lifecycle smoke commits a real fixture effect and short return, records an
explicit typed failure, then restores with another call started and a final call
still queued. It checks the four distinct states, synthetic protocol repair,
stable provider IDs, refusal to replay closed calls, and serialization/replay of
the recorded continuation bytes. It also checks that error-shaped text alone
does not become a typed failure.
It uses no model, network endpoint, pytest, or terminal.

The fixture calls `Session` from trusted Python code and uses temporary storage;
it does not exercise a model tool sandbox. A production consumer must keep the
store outside all effective tool read/write scopes and enforce that separation
for file tools and subprocesses, including path aliases. Route retained-frame
retrieval through `re_read`. The [binding contract](../../README.md) describes
the directory, checkpoint, and current-policy inputs available to the host.
Passing stored configuration back to a restore method in this smoke tests
state round-tripping; a production host derives current authorization from its
own execution policy before restoring.

License: Apache-2.0.
