# newt-scheduler Example

Shows task scheduling:

```rust
use newt_scheduler::Scheduler;
let mut sched = Scheduler::new();
sched.schedule("task1", 1000, || println!("Task ran!"));
sched.run();
```