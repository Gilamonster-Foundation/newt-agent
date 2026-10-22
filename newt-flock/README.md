# newt-flock

Newt's side of an [agent-mesh](https://github.com/Gilamonster-Foundation/agent-mesh)
flock: it follows the flock as one of its members, hands out only frames that
agent-mesh's own reader has verified and saved, and turns them into tasks, open
asks and signed replies. Verification is upstream's; this crate adds none of its own.

**Scope.** Unix only. Newt holds no flock key: the member's session process
signs both the stream challenge and every reply over its control socket. The
crate depends on agent-mesh with the server and emit features off. `newt-tui`
has integration hooks for it behind its off-by-default `flock` feature (a task
intake and `/asks` / `/answer`), but the launcher does not compose them yet:
nothing starts them, so `/asks` reports that no flock is connected.

**Use.** `Inbox::start(Config { genesis, serve, session, state_dir, retry })`
checks the session against the pinned genesis, opens the reader's checkpoint in
`state_dir` (created `0700`; a directory others can enter, or a symlink, is
refused), and follows in catch-up passes every `retry`. Read `Inbox::events()`;
`stop()` or dropping the inbox ends the worker.

**Build.** It is a workspace member but not a default member, because it is
Unix only. Build it with `cargo build -p newt-flock`.

License: Apache-2.0.
