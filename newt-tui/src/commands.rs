//! Per-group slash-command handlers, split out of the `dispatch_slash` mega-match
//! in `lib.rs` (#1096 functional-cohesion pass). Each submodule owns one
//! cohesive command family; `dispatch_slash` is a pure router that hands a
//! family's command names to that module's `dispatch()`. Pure code-motion —
//! behavior is identical to the inline arms these modules replace.

pub(crate) mod crew;
pub(crate) mod meta;
pub(crate) mod model;
pub(crate) mod settings;
