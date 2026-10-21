# newt-flock

The read side of newt's flock inbox: it follows an
[agent-mesh](https://github.com/Gilamonster-Foundation/agent-mesh) flock as one
of its members and hands out only frames that agent-mesh's own reader has
verified and saved. Verification is upstream's; this crate adds none of its own.

**Scope.** Unix only, read only. Newt holds no flock key: the member's session
process signs the stream challenge over its control socket. The crate depends on
agent-mesh with the server and emit features off. Nothing in the application
uses it yet.

**Use.** `Inbox::start(Config { genesis, serve, session, state_dir, retry })`
checks the session against the pinned genesis, opens the reader's checkpoint in
`state_dir` (created `0700`; a directory others can enter, or a symlink, is
refused), and follows in catch-up passes every `retry`. Read `Inbox::events()`;
`stop()` or dropping the inbox ends the worker.

**Build.** It is a workspace member but not a default member, because its
agent-mesh dependencies are git-pinned until the flock crates are on crates.io.
Build it with `cargo build -p newt-flock`.

License: Apache-2.0.
