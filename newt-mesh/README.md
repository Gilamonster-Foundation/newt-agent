# newt-mesh

Mesh integration for newt-agent — announce capabilities, ask peers for
inference.

- `NewtMeshService` — bind a mesh-listening newt that answers
  `InferenceRequest`s on the agent-mesh bus.
- `MeshAsker` — client to send `InferenceRequest`s to a peer newt and await
  its `InferenceReply`.

Wire types live in the `protocol` module. Note this crate path-depends on a
sibling `agent-mesh` checkout and is excluded from the default workspace
build.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
small, fast, local-first agentic coder.

## License

Apache-2.0
