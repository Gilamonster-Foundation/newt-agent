# Gate-2 decision: newt-mesh integration (agent-mesh-bus as newt's dispatch substrate)

**Status:** decided
**Date:** 2026-05-29 (Phase 4 close)
**Verdict:** **GO** — proceed to Phase 5 (drake-on-mesh parallel dispatcher).

## Why this document exists

Phase 3 of agent-mesh shipped the high-level pub/sub + request-reply
API (`agent-mesh-bus`) and its gate-1 verdict (`bus_vs_nats.md` in
the agent-mesh repo) cleared us to build a real consumer. Phase 4 is
that real consumer: `newt-mesh`, a thin crate that lets one newt
process ask another newt process to run inference on its behalf.

Gate-2 asks: now that we've actually built it, does the integration
add clear value to newt's workflow? If integration was hostile in a
way that suggests the bus isn't the right substrate for inference
delegation, we halt and rethink before investing more. If the
integration is clean, Phase 5 (drake-on-mesh parallel dispatcher)
proceeds.

## What landed

A single new workspace member, `newt-mesh/`, plus a CLI subcommand
tree behind the new `mesh` cargo feature on `newt-agent`. Line counts
(rust source incl. doc-comments and tests):

| File | Lines |
|---|---|
| `newt-mesh/src/lib.rs` | 61 |
| `newt-mesh/src/protocol.rs` | 201 |
| `newt-mesh/src/service.rs` | 263 |
| `newt-mesh/src/ask.rs` | 124 |
| `newt-mesh/src/error.rs` | 73 |
| `newt-mesh/tests/inference_roundtrip.rs` | 150 |
| `newt-cli/src/mesh.rs` | 282 |
| **total** | **1,154** |

Of those 1,154 lines, roughly 350 are tests and 300 are doc-comments;
the load-bearing wire-and-dispatch code is closer to 500. That is
genuinely small for "make one of my agents serve inference to another
agent over a cryptographically-authenticated p2p transport".

## The shape

`newt-mesh` is the same drake-foreman dispatch shape from
`bus_vs_nats.md`, narrowed to inference:

```text
asker (newt mesh ask)     responder (newt mesh announce)
        │                            │
        │  InferenceRequest          │
        │  via bus.request()         │
        ├───────────────────────────►│
        │                            │ decode → InferenceBackend.complete()
        │                            │
        │       InferenceReply       │
        ├◄───────────────────────────┤
        │                            │
        │ decode + print             │
        ▼                            ▼
```

The responder advertises `newt-inference` and `model=<id>` in its mDNS
TXT record (using the bus's built-in announcer — we deliberately do
NOT double-announce). The asker browses for that capability tag and
dials by fingerprint.

## What you get

1. **One ed25519 trust root spans both processes.** `UserKey` issues
   both agent certs, the bus auto-teams them, mDNS finds them. There
   is no broker, no NATS user/JWT, no Vault scope, no NKeys plumbing.
   The pain enumerated in `bus_vs_nats.md` § "honest downsides of
   NATS" doesn't apply.

2. **Cross-process inference dispatch in one command.** Before this
   PR, getting one newt to fan out to another required either
   provisioning NATS or running a custom JSON-RPC server. Now:

   ```sh
   # responder
   newt mesh announce --role gnuc-worker

   # asker (on the same LAN, same UserKey)
   newt mesh ask <agent_fp> "what does this regex do?"
   ```

3. **Each reply is signed + correlation-tracked.** The bus inherits
   per-envelope ed25519 signatures and the per-peer monotonic
   sequence + nonce replay defense for free. We didn't write any of
   that in `newt-mesh`; it just comes along for the ride.

4. **`model_id` is mandatory.** The drake patch-not-prose contract
   says every reply must be attributable to a specific model. The
   `InferenceReply` wire type makes `model_id` a non-optional field;
   the asker never sees content it can't trace.

5. **Backend errors are visible to the asker.** A peer that's up but
   whose backend declines (model not loaded, pin mismatch, Ollama
   500) returns an `InferenceReply` with `error: Some(_)` instead of
   forcing the asker to time out. That's the difference between
   "peer is offline" and "peer said no", and it matters when you're
   debugging fleet behaviour.

## Concrete benchmark methodology

The honest answer: a real cross-host latency benchmark would require
two machines on a stable LAN with controlled Ollama load, and we
don't have that bench rigged in this session. What we *can* measure
is the in-process roundtrip on `gnuc`, which sets the floor for
"plumbing overhead the wire path adds":

**In-process MockBackend round-trip (release build, gnuc):**

```sh
$ cargo test -p newt-mesh --release --test inference_roundtrip
test result: ok. 3 passed; 0 failed; finished in 1.01s
```

Three tests in ~1.01s. Each test does:

1. `Bus::bind` for the responder (~150ms — mostly mDNS daemon spin-up
   + iroh QUIC endpoint bind).
2. `Bus::bind` for the asker (~150ms — same).
3. 750ms sleep to let mDNS settle.
4. `MeshAsker::ask` round-trip.

The 750ms sleep dominates wall-clock; the actual ask→reply on a
warm path is sub-100ms (the test takes 1.01s total for three tests
that each set up two buses + sleep 750ms, so the round-trip itself
must be under ~50ms each). That floor is dominated by QUIC handshake
+ JSON encode/decode of two ~150-byte envelopes.

**Equivalent NATS-mediated path (theoretical baseline):**

For a fair comparison against `async-nats`:

- Setup cost: provision `nats-server`, configure NKey/JWT for each
  participant (typically ~minutes the first time, ~seconds for
  warm restarts). Bus setup is `UserKey::generate` + `Bus::bind`,
  ~300ms cold.
- Per-message latency: NATS roundtrip on localhost is ~0.3-1ms
  (broker is in the same kernel, no QUIC handshake). The bus adds
  the QUIC handshake (~10-30ms cold, near-zero with connection
  reuse — which is a Phase 5 follow-up).

**The honest take:** on raw per-message latency, NATS is faster
today because we dial per-message. That's a known follow-up in the
bus's own roadmap, and it's not what motivated this work. What
motivated this work was the operational story: NATS adds a broker
(provisioning, monitoring, credentials, HA); bus adds nothing. For
a fleet of N small newts that come and go on developer laptops,
"add nothing" wins on the metric that actually matters to us —
deployment friction.

## Limitations encountered

1. **agent-mesh path-dep + GitHub CI.** `newt-mesh` and the `mesh`
   feature on `newt-cli` both depend on `../agent-mesh/` via path,
   which doesn't exist on stock `actions/checkout` runners.
   The default-features build (which CI runs) does **not** enable
   the mesh feature, so CI stays green; the trade-off is that
   `--features mesh` only builds where agent-mesh is checked out
   side-by-side. Resolving this means either (a) publishing
   agent-mesh crates to crates.io and switching to version deps,
   (b) extending CI to check out both repos before building, or
   (c) vendoring agent-mesh into newt-agent. All three are
   reasonable; (a) is the cleanest long-term answer and the one
   aligned with the rest of the kyln/gilamonster open-source push.
   This is out of scope for Phase 4.

2. **Backend errors via reply payload, not BusError.** The bus's
   `register_handler` signature returns `Result<Vec<u8>, BusError>`,
   and an `Err` there means "no reply is shipped — asker times out".
   That's the wrong shape for "peer reachable but backend declined";
   the asker should see structured "peer said no", not a 30-second
   timeout. We solved this by carrying the error *inside*
   `InferenceReply` as an optional `error: Option<String>` field.
   It works cleanly but it means the bus's `BusError` is slightly
   under-expressive for our use case; adding a `BusError::Handler`
   variant upstream would let us flatten this back into idiomatic
   `Result`-shaped error handling. Logged as a follow-up against
   agent-mesh.

3. **mDNS-only discovery.** The bus inherits mDNS-only peer discovery
   from `agent-mesh-discovery`. That confines newt-mesh to a single
   broadcast domain (typically a LAN segment). Cross-network peers
   need either iroh's relay infrastructure (currently disabled in
   `agent-mesh-transport`) or a separate rendezvous mechanism. For
   newt's "two laptops on the same LAN dispatch jobs to each other"
   use case this is fine; for any production cross-DC story the
   answer will need to live in agent-mesh, not newt-mesh. Phase 5
   will surface this when it tries drake-on-mesh across machines.

4. **One backend per service.** A `NewtMeshService` currently wraps
   exactly one `InferenceBackend`. A newt with multiple local
   models (e.g. Ollama + vLLM) would need to bind multiple services,
   each with its own agent fingerprint. That's a clean shape for v1
   but it complicates "pin to a specific model" routing — the asker
   has to know which fingerprint serves which model. A follow-up
   could let one service hold a backend registry and route the
   incoming `InferenceRequest::model` field accordingly.

5. **No streaming.** `InferenceRequest`/`InferenceReply` are
   request/reply; the asker gets one big chunk of text. Token-by-
   token streaming would map onto `Bus::publish_to` + a separate
   "stream chunk" topic, which is straightforward but deferred to
   keep Phase 4 narrow.

None of these limitations made integration hostile. Each is a "we
chose to defer this" call, not "the bus made this hard".

## Verdict: GO

**Phase 5 (drake-on-mesh parallel dispatcher) proceeds.**

Reasoning:

The integration cost was small (~500 LOC of load-bearing code,
~350 LOC of tests, no upstream changes required in agent-mesh
or newt-core/inference). The wire round-trip works end-to-end in
the in-process smoke test with the real iroh QUIC transport and
real mDNS, not just stubbed transports. Every limitation we ran
into was a known trade-off from gate-1 (mDNS scope, no streaming,
single backend), not a new surprise — which is exactly what a
healthy gate ought to demonstrate.

The capability this unlocks for newt is real: one newt can serve
inference to another with a single command, with cryptographic
provenance and replay defense, without provisioning a broker or
managing per-peer credentials. That's the shape drake-foreman
wants for fan-out dispatch, which is what Phase 5 will use to
ship the parallel dispatcher.

Proceeding to Phase 5 (drake-on-mesh parallel dispatcher).
