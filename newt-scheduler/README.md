# newt-scheduler

`newt-scheduler` is Newt-Agent's availability-adaptive orchestration library.
Its `BackendPool` selects a live inference backend by tier and optional model
pin, distinguishes busy backends from unavailable ones, and supports
health-aware failover.

The crate also provides the crew, team, and panel control loops used to route
specialized roles across heterogeneous models. Inference and workspace effects
sit behind traits, so orchestration policy can be tested with deterministic
in-memory implementations.

The optional `embedded` feature enables dispatch to Newt-Agent's in-process
inference backend.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## License

Apache-2.0
