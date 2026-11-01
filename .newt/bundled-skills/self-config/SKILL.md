---
name: self-config
description: Propose an edit to newt's own config.toml (an env var passed to commands, a setting) with propose_config_edit. The operator sees the diff and must choose apply; nothing is written otherwise.
when_to_use: When a task fails because of newt's own configuration (a command lacks an environment variable such as BROWSER, a setting blocks the work) and the fix is a config.toml change; when the operator asks you to change a newt setting.
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: [] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Self-config — propose, the operator applies

`config.toml` is outside the write fence. File tools and the shell cannot change
it, and should not try. `propose_config_edit` is the one path.

1. Name the smallest edit that fixes the problem: one dotted key, such as
   `shell.env_passthrough`.
2. To add to a list, pass `append: true` with the new entries only. Entries
   already present are skipped. Without `append`, the value replaces the key.
3. Give a one-line `reason`. The operator reads it beside the diff.
4. Call it once. The result says `applied`, `declined`, or `no operator to
   confirm`. Do not resend a declined or refused edit; tell the operator what
   you would have changed and let them decide.
5. An applied edit takes effect when newt next reads its config, which may be the
   next session.

Example, letting commands open a browser:

```json
{"key": "shell.env_passthrough", "value": ["BROWSER", "DISPLAY"], "append": true,
 "reason": "commands need BROWSER and DISPLAY to open a browser"}
```

An unknown key, or a value of the wrong type, is refused before the operator is
asked. Headless runs (eval, ACP) have no operator, so the edit is refused there.
