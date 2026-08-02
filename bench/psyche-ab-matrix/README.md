# Nemotron Psyche × OCAP qualification matrix

This harness runs a declared set of self-verifying tasks across Newt's four
Psyche postures and both OCAP lanes. Its default `qualification` mode is a
release-evidence collector: every requested cell runs, the complete report is
rendered, and the process then fails closed if any result, contract record, or
provenance fact is missing or inconsistent.

The runner is intended for the Nemotron reference lane first, with a separate
llama.cpp portability lane. It does not claim a performance result merely
because inference completed.

## What cognition means on Chat Completions

Chat Completions can carry Newt's cognition policy when the endpoint explicitly
declares that it implements the required extensions. This runner's `nemotron`
capability profile writes the following contract into both the live and
loopback configs:

```toml
[backends.capability]
reasoning_replay_scope = "current_user_turn"

[backends.capability.chat_completions]
cognition = true
chat_template_kwargs = true
parallel_tool_calls = false
bounded_reasoning_continuation = true
```

Unknown OpenAI-compatible endpoints remain conservative in Newt and receive
none of those fields. The behavior is capability-driven, not inferred from a
model display name.

Before contacting the inference server for a matrix cell, qualification mode
drives the candidate Newt binary for two rounds against a loopback server with
`--cognition contemplating`. It retains `preflight/first-request.json` and
`preflight/second-request.json`. The first request must contain:

- exactly the runner's qualification user prompt and no replay marker;
- `max_tokens = 16000`, `temperature = 0.6`, and `top_p = 0.95`;
- thinking enabled with history-thinking truncation;
- `parallel_tool_calls = false`; and
- a non-empty set of uniquely named, structurally valid function tools with
  `tool_choice = "auto"`.

The loopback server then returns a reasoning-only `length` response. The second
request must preserve the first message list byte-for-structure, append exactly
one assistant reasoning message, and contain that marker exactly once. This
proves the current-user-turn replay and bounded continuation paths as well as
initial policy projection. The loopback server has its own bounded lifetime,
and every externally timed subprocess also receives a forced-kill grace period,
so incomplete capture and TERM-resistant children fail instead of hanging the
runner.

This proves that the rendered capability profile reached the real wire path.
The live cells and their contract records show that the target server accepted
the requests; they do not independently prove how the server applied each
generation field internally.

## Qualification prerequisites

Provide facts from the actual inference-server launch. There are deliberately
no qualification defaults for these values:

| Environment variable | Required fact |
|---|---|
| `MODEL_ENDPOINT` | Base URL of the exact inference process under test |
| `MODEL_DIGEST` | Immutable identity in exactly one accepted form: 40 hex (pinned revision), 64 hex, or `sha256:<64 hex>` |
| `CONTEXT_WINDOW` | Full context window served by the inference process |
| `SERVER_KIND` | `vllm` or `llama_cpp` |
| `SERVER_VERSION` | Exact inference-server version/build identity |
| `CHAT_TEMPLATE_ID` | Template identity; llama.cpp requires `sha256:<64 lowercase hex>` over the UTF-8 bytes of `/props.chat_template` |
| `TOOL_PARSER_ID` | Tool parser selected at server launch |
| `REASONING_PARSER_ID` | Reasoning parser selected at server launch |
| `SERVER_LAUNCH_MANIFEST` | JSON record captured from the actual server launch; schema below |

Also required on the host:

- the exact candidate `newt` executable;
- Python 3.9 or newer, `curl`, GNU `timeout`, `git`, and `just`;
- reachability to the OpenAI-compatible endpoint; and
- an executable `verify.sh` for every task (and executable `setup.sh` where used).

For vLLM qualification, the reference lane is enforced as
`TOOL_PARSER_ID=qwen3_coder` and `REASONING_PARSER_ID=nano_v3`, matching the
[NVIDIA Nemotron model card](https://huggingface.co/nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16).
The runner proves the requested model appears in `/v1/models` and compares the
stable vLLM `/version` JSON with `SERVER_VERSION`. It deliberately does not use
the development-only extended introspection endpoint. The llama.cpp lane calls
`GET /props?model=<MODEL_ID>` (using URL encoding in router mode), retains the
response, and requires all three stable fields: `build_info == SERVER_VERSION`,
`default_generation_settings.n_ctx == CONTEXT_WINDOW`, and
`sha256(UTF-8(/props.chat_template)) == CHAT_TEMPLATE_ID`. The `/props` schema
does not expose stable tool/reasoning parser IDs, so those remain assertions
bound to the retained launch manifest rather than invented probe evidence.

The launch manifest is operator-attested provenance from the server launch
workflow. Qualification parses it, compares every field with the environment
and run manifest, copies it into the bundle, and records its SHA-256. This
binding detects inconsistent or subsequently changed declarations, but it does
not independently prove the weights, context, template, or parser flags used by
the inference process. Only the retained endpoint probes described above are
independently observed by this harness:

```json
{
  "schema_version": 1,
  "model_id": "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16",
  "model_digest": "<pinned-weights-or-revision-digest>",
  "context_window": 65536,
  "server_kind": "vllm",
  "server_version": "<exact-vllm-version>",
  "chat_template_id": "<template-name-at-revision-or-digest>",
  "tool_parser_id": "qwen3_coder",
  "reasoning_parser_id": "nano_v3"
}
```

## Run the vLLM reference lane

Build the candidate once, then point `NEWT` at that immutable binary:

```bash
cargo build --release -p newt-agent --bin newt

NEWT="$PWD/target/release/newt" \
MODEL_ENDPOINT=http://127.0.0.1:8000 \
MODEL_ID=nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16 \
MODEL_DIGEST=<pinned-weights-or-revision-digest> \
CONTEXT_WINDOW=65536 \
SERVER_KIND=vllm \
SERVER_VERSION=<exact-vllm-version> \
CHAT_TEMPLATE_ID=<template-name-at-revision-or-digest> \
TOOL_PARSER_ID=qwen3_coder \
REASONING_PARSER_ID=nano_v3 \
SERVER_LAUNCH_MANIFEST=/secure/run-records/nemotron-vllm-launch.json \
./bench/psyche-ab-matrix/run-matrix.sh
```

The model's native vLLM launch should select the `qwen3_coder` tool parser,
`nano_v3` reasoning parser, the declared chat template, and an output allowance
large enough for Newt's contemplating policy. `CONTEXT_WINDOW` is passed to
every solve; Newt composes it with the cognition output reserve.

## Run the llama.cpp portability lane

Use the digest of the exact Q8_0 GGUF and identities reported by that llama.cpp
build and launch:

```bash
NEWT="$PWD/target/release/newt" \
MODEL_ENDPOINT=http://127.0.0.1:8080 \
MODEL_ID=<served-gguf-model-id> \
MODEL_DIGEST=<gguf-sha256> \
CONTEXT_WINDOW=32768 \
SERVER_KIND=llama_cpp \
SERVER_VERSION=<exact-props-build_info> \
CHAT_TEMPLATE_ID=sha256:<sha256-of-exact-props-chat_template-utf8> \
TOOL_PARSER_ID=<llama.cpp-tool-parser-identity> \
REASONING_PARSER_ID=<llama.cpp-reasoning-parser-identity> \
SERVER_LAUNCH_MANIFEST=/secure/run-records/nemotron-llama-launch.json \
./bench/psyche-ab-matrix/run-matrix.sh
```

Do not reuse the vLLM parser values unless that is literally how the llama.cpp
process was configured. These fields are provenance assertions, not labels for
making unlike servers appear equivalent.

Compute the llama.cpp template identity from the exact decoded string returned
for the requested model; do not hash the surrounding JSON serialization or a
locally reconstructed template. For example, after retaining `/props`, hash
`chat_template.encode("utf-8")` and prefix the lowercase digest with `sha256:`.

## Matrix axes

| Posture | How it is set | Effective intent |
|---|---|---|
| `baseline` | no override | cognition default, tenacity standard, no crew |
| `tenacity` | `--tenacity relentless` | cognition default, relentless, crew off |
| `crew` | `NEWT_TEAM=1` | cognition default, standard, crew on |
| `obsessive` | `--obsessive` | contemplating + relentless + crew |

`default` means Newt sends no cognition selection. The inference server retains
its own default behavior, which may include reasoning for Nemotron.

| OCAP | How it is set | Lane |
|---|---|---|
| `off` | `--non-interactive true` | full-access/yolo bench lane |
| `on` | `--confined` + `NEWT_BENCH_OCAP=on` | workspace-fenced Newt write tools |

Defaults are all four postures, both OCAP modes, and all directories beneath
`tasks/`. Qualification requires that complete 4 × 2 axis. Subset `POSTURES` or
`OCAP_MODES` are accepted only with `MODE=exploratory`. `TASKS_DIR` may point to
another task set with the same layout; the runner canonicalizes it before task
discovery and records the canonical path. Task directories, required instruction
and verification files, and an optional setup file when present may not be
symlinks.

## Gate semantics

The runner never stops merely because one solve or task verification failed.
It records every declared cell and renders `matrix.md`, then exits nonzero if a
required cell:

- failed its task verifier or returned a nonzero solve code;
- did not report `completed`;
- lacked exactly one contract-v1 record;
- did not point to its exact runner-owned event path
  `events/{posture}-{ocap}-{task}.jsonl`, or reused another cell's evidence;
- reported a requested/effective model mismatch;
- disagreed on digest, context window, max rounds, backend, or OCAP lane;
- reported a tenacity other than `standard` for baseline/crew or `relentless`
  for tenacity/obsessive;
- reported cognition other than `default` for baseline/tenacity/crew or
  `contemplating` for obsessive;
- reported crew other than `off` for baseline/tenacity or `on` for
  crew/obsessive; or
- lacked qualification provenance or either structurally validated preflight request.

The validator is explicitly bound to the mode selected when the runner starts,
so changing the retained manifest from `qualification` to `exploratory` cannot
downgrade the gate. Every retained gate artifact must also be a regular file
reached without traversing a symlink; the validator rejects symlinked leaves and
parent directories.

The additive contract-v1 posture fields are
`effective_config.cognition = default|<level>` and
`effective_config.crew = off|on`. They are emitted from the same resolved
runtime snapshot that configures the headless driver; the runner does not
infer them from its own command line.

It also fails if the Newt binary or any harness/task source file changes between
the beginning and end of the matrix. The binary path, version output, SHA-256,
per-source hashes, aggregate harness hash, source commit, and actual initial
dirty state are retained and summarized in the report. Each task workspace is
separately initialized and committed as a real Git repository with a
`just check` recipe before Newt starts. Existing project markers are preserved.
Before Newt can mutate the workspace, the runner records a per-cell baseline
proof containing the canonical Git root, commit and tree IDs, clean state,
committed Justfile blob and SHA-256, discovered recipes, `just --dry-run check`
digest, and canonical verifier identity. The final gate validates that retained
proof against the pinned task-source snapshot; it does not need to trust the
post-solve workspace or its `.git` directory.

## Output and retention

Qualification output defaults to `qualification-runs/<UTC stamp>/` and
exploratory output to `runs/<UTC stamp>/`. Both are git-ignored because bundles
contain prompts, model output, workspaces, endpoint details, key-file paths, and
provenance that may be sensitive. They are retained locally; move completed
qualification bundles to access-controlled artifact storage rather than
committing them. An explicit `OUT` must be new or empty so an earlier JSONL
contract cannot contaminate the run.

Each run retains:

- `manifest.json`, `provenance.json`, the copied+hashed
  `server-launch-manifest.json`, `harness-sources.json`, and the exact
  `config.toml`;
- raw server identity responses (`server-models.json` plus kind-specific
  probes);
- the loopback config, both captured requests, Newt trace, and JSONL events;
- one raw JSONL event stream and text trace per matrix cell;
- one immutable pre-solve Git/Justfile baseline proof per matrix cell;
- `results.csv`, `matrix.md`, and `validation.txt`; and
- the task workspaces and setup traces retained only for failure diagnosis.

Do not promote only the CSV or Markdown summary. The raw JSONL, request,
configuration, server probes, and provenance files are part of the evidence.
When `MODEL_KEY_FILE` is set, probe authorization is fed to `curl` over stdin;
the bearer token is neither placed in process arguments nor copied into the run
bundle.

## Exploratory mode

Use an explicit exploratory run for an unknown endpoint or a Responses API leg:

```bash
MODE=exploratory \
CAPABILITY_PROFILE=none \
MODEL_API=responses \
MODEL_ENDPOINT=https://api.openai.com \
MODEL_ID=<model> \
MODEL_KEY_FILE=<key-file> \
BACKEND_NAME=<backend> \
./bench/psyche-ab-matrix/run-matrix.sh
```

Exploratory mode does not require the Nemotron provenance profile or loopback
assertion, but it still rejects failed/missing cells, malformed contract counts,
and effective-model mismatches. It is not qualification evidence.

## Offline harness tests

The validator includes a complete positive fixture and independent negative
mutations for verification, solve, cell-count, contract, model, provenance,
launch-manifest, server-probe, axis, posture-tenacity, binary/source-identity,
posture-cognition/crew, canonical per-cell event ownership, retained evidence,
workspace-baseline, and preflight failures. Runner tests use a real loopback
socket with fake inference/Newt executables, exercise bounded zero/one/two-POST
preflight lifetimes, and verify that a failed model identity still produces a
complete report before returning nonzero.

```bash
python3 -m unittest discover -s bench/psyche-ab-matrix/tests -p 'test_*.py'
bash -n bench/psyche-ab-matrix/run-matrix.sh
shellcheck bench/psyche-ab-matrix/run-matrix.sh
```
