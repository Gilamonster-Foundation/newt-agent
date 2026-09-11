"""Check the actual external consumer graph, including its Python binding edge."""

import subprocess
from pathlib import Path

manifest = Path(__file__).with_name("Cargo.toml")
tree = subprocess.check_output(
    ["cargo", "tree", "--locked", "--manifest-path", str(manifest), "--edges", "normal,build",
     "--prefix", "none", "--format", "{p}"],
    text=True,
)
names = {line.split()[0] for line in tree.splitlines() if line.strip()}
assert {"smart-harness-consumer", "agent-harness-py", "agent-frame", "agent-harness", "pyo3"} <= names
forbidden = {"newt-core", "newt-tui", "newt-agent", "newt-agent-py", "reqwest", "tokio"}
assert not names & forbidden, f"foreign consumer acquired {sorted(names & forbidden)}"
