---
name: dgx-fleet-default
description: Manage the fleet-wide default model alias (`default`) on a llama.cpp router running in models-preset mode — report what the alias points at, point it at another registered preset, restart the router, warm the alias, verify it. The alias is the one switch that changes every local agent's default model; client configs are out of scope.
when_to_use: When asked to "set the default model for the fleet", "make <model> the default", "what is default pointing at", "flip the default model", or "warm the default model"; when a default must change for every client at once and editing each machine's config is the wrong answer.
version: 1.0.0
license: Apache-2.0
---

# Fleet default model alias (`default`)

Companion to `dgx-spark-admin` (building and serving) and `dgx-model-switch`
(co-host ↔ big-model memory modes). This skill covers one thing: the router
side of the alias every local client names as its model. Client configs
(pi, newt, gila, LiteLLM) are rewritten by a separate process; do not edit
them from here.

## Why an alias

The router's preset file (`--models-preset models.ini`) names each served
model by its section header, so a section `[default]` serves a model id
`default`. Changing what the fleet gets is editing that section's `model =`
path and restarting the router.

## Resolve the router host — never hardcode it

Hostnames belong in local config only. Take them from what is already there:

```bash
EP=$(grep -m1 '^endpoint' ~/.newt/backends/<router-backend>.toml | sed 's/.*"\(.*\)"/\1/')
HOST=$(echo "$EP" | sed -E 's#https?://([^:/]+).*#\1#')
```

File edits go over `ssh "$HOST"`; the API is `curl "$EP"`. The preset path
and launch args come from the running process, not from memory:

```bash
ssh "$HOST" 'pgrep -af "llama-server --models-dir" | head -1'
PRESET=<the --models-preset path from that line>
```

## Inspect

```bash
curl -s "$EP/models" | jq '.data[] | select(.id=="default") | {id, status}'
ssh "$HOST" "grep -n -A2 '^\[default\]' $PRESET"
ssh "$HOST" "grep -n '^\[' $PRESET"          # every registered preset id
```

Report three facts: what `default` points at, whether it is loaded, and
which real preset serves the same GGUF (the model's true name).

## Point `default` at a registered preset

Only point the alias at a path that already has its own section, so it
always resolves to a model the router knows how to serve. Idempotent: adds
the section if absent, rewrites its `model =` line if present.

```bash
TARGET=<real preset id, e.g. the one the operator named>
GGUF=$(ssh "$HOST" "awk -v s='[${TARGET}]' '\$0==s{f=1;next} /^\[/{f=0} f && /^model *=/{sub(/^model *= */,\"\");print;exit}' $PRESET")
[ -n "$GGUF" ] || { echo "no preset [$TARGET] in $PRESET"; exit 1; }
ssh "$HOST" "python3 - '$PRESET' '$GGUF'" <<'PY'
import re,sys
p,gguf=sys.argv[1:]; s=open(p).read()
sec=f"[default]\nmodel = {gguf}\n"
if re.search(r'^\[default\]', s, re.M):
    s=re.sub(r'^\[default\]\n(?:(?!\[).*\n)*', sec, s, count=1, flags=re.M)
else:
    s=s.rstrip('\n')+"\n\n"+sec
open(p,'w').write(s); print(s[s.index('[default]'):][:120])
PY
```

If the target section carries `ctx-size` or `reasoning-budget`, copy those
lines into `[default]` too: the alias is a separate process and inherits
nothing.

## Restart the router

The preset is read at startup; there is no reload. Two cases:

```bash
# systemd unit installed (dgx-spark-admin's llama-router.service):
ssh "$HOST" 'systemctl restart llama-router'
# bare process (parent is init): relaunch with the SAME args it had
ssh "$HOST" 'ARGS=$(pgrep -af "llama-server --models-dir" | head -1 | cut -d" " -f2-); \
  pkill -f "llama-server --models-dir"; sleep 3; \
  nohup $ARGS >/tmp/llama-router.log 2>&1 & disown; echo relaunched: $ARGS'
until curl -sf "$EP/health" >/dev/null; do sleep 2; done
```

Restarting evicts every loaded model. List what is loaded first and say so
before doing it when other work may be in flight:
`curl -s "$EP/models" | jq -r '.data[] | select(.status.value=="loaded").id'`.

## Warm and verify

```bash
curl -s -X POST "$EP/models/load" -H 'content-type: application/json' -d '{"model":"default"}'
curl -s "$EP/v1/chat/completions" -H 'content-type: application/json' \
  -d '{"model":"default","messages":[{"role":"user","content":"ready?"}],"max_tokens":4}' | jq -r '.model, .choices[0].message.content'
newt dgx models | grep -w default
```

## Caveats to state in the report

- **Double load.** The alias is its own process. A client using the real
  name alongside the fleet's `default` loads the weights twice; `--models-max`
  then evicts something.
- **Warmth is per id.** `default` warm ≠ the real name warm.
- **Benchmarks never use `default`.** A scoreboard row names the weights it
  measured; keep bench profiles on real preset ids.
- **Thinking models.** If `default` points at a preset that reasons by
  default, add `reasoning-budget = 0` to `[default]` or every client pays the
  thinking tax on every turn.
