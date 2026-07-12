//! Per-group slash-command handlers, split out of the `dispatch_slash` mega-match
//! in `lib.rs` (#1096 functional-cohesion pass). Each submodule owns one
//! cohesive command family; `dispatch_slash` routes a family's command names to
//! that module's `dispatch()`. Pure code-motion — behavior is identical to the
//! inline arms it replaces.

pub(crate) mod model;
