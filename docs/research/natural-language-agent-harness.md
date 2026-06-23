# Reference card — "Natural-Language Agent Harnesses"

| | |
|---|---|
| **Title** | Natural-Language Agent Harnesses |
| **Authors** | Linyue Pan, Lexiao Zou, Shuo Guo, Jingchen Ni, Hai-Tao Zheng |
| **arXiv** | [2603.25723](https://arxiv.org/abs/2603.25723) ([HTML](https://arxiv.org/html/2603.25723v1)) — submitted 2026-03-26 (rev. 2026-05-18) |
| **PDF** | <https://arxiv.org/pdf/2603.25723> — *not committed* (gitignored via `docs/research/*.pdf`) |
| **Why we have it** | Phase 26 follow-up — agent-harness design as a first-class, ablatable object; informs the **experimentation controls** (`/context feature`, `/context stats`, #588) |

## What it's about
Proposes representing the **agent harness** (the external execution system that
organizes a task run) as an **executable natural-language document** rather than
controller code buried in the implementation. Introduces **Natural-Language Agent
Harnesses (NLAHs)** + an **Intelligent Harness Runtime (IHR)** that interprets
those documents into agent calls, state updates, and validation gates. Across
several benchmarks, IHR-executed NLAHs reach parity with hand-coded harnesses
using substantially shorter, more inspectable static policies.

**Main contribution:** turns harness design from an implicit implementation
detail into an explicit, **editable, comparable, transferable, ablatable**
artifact — "harness as a first-class research concern."

## Relevance to newt
Directly motivates the Phase 26 experimentation layer: if harness/context
policies are first-class and ablatable, then per-feature toggles
(`/context feature <name> on|off`) + comparison telemetry (`/context stats`,
#588) are the mechanism for ablating newt's own context-management features and
comparing them empirically. Contrast with the *Code as Agent Harness* survey
([2605.18747](./2605.18747-code-as-agent-harness.md)) which catalogs the
techniques; this paper is about making the harness itself inspectable/editable.

## Fetch the PDF locally (not in git)
```bash
curl -L -o docs/research/2603.25723v1.pdf https://arxiv.org/pdf/2603.25723
```
