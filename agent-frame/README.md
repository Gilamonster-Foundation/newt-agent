# agent-frame

A pure Rust derivation and causal-event kernel. It has no filesystem, async,
HTTP, or inference dependency. The `agent-harness` crate adds durable storage,
ordered request projection, navigation policy, and restoration; `agent-harness-py`
exposes the same admission boundaries to Python through PyO3.

`Unit` and `Derivation` identify source spans and verify proof-carrying elision.
`RootEvent` identifies admitted source material. `Packet` remains a chain with
at most one predecessor and an ordered list of unit identities. `Event` adds causal DAG links for
observations, interventions, verdicts, retrieval, and elision without changing
packet geometry. Every identity and canonical encoding comes from
`content-addressable`.

Decode an event into `RawEvent`, then call `Event::admit` with a resolver for all
referenced causal events. Construction and admission enforce the same origin,
source-membership, and derivation-depth rules. Observations have depth zero;
generated material has depth at most one; retrieval and elision preserve source
origin and depth. The host separately resolves root and payload bytes, verifies
elision units, and enforces authority. A generated reply verdict proves its
schema and provenance; it does not establish factual truth or task correctness.

Licensed under the workspace Apache-2.0 license.
