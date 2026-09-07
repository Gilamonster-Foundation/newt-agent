//! Test families for [`super`] (`newt_core::ocap`), split out of `ocap.rs`.
//! One file per asserted security property, mirroring the four test modules
//! this file replaces — those names already partitioned by property, and
//! `docs/security/ocap-deviations.md` cites them in exactly those groups.

// A glob RE-EXPORT, not a plain glob: each moved body opens with
// `use super::*;` written when `super` was `ocap`, and a private glob binding
// is not nameable by path from a child module.
pub(crate) use super::*;

#[cfg(test)]
#[path = "disclosure.rs"]
mod disclosure;
#[cfg(test)]
#[path = "report.rs"]
mod report;
#[cfg(test)]
#[path = "separation_of_duties.rs"]
mod separation_of_duties;
#[cfg(test)]
#[path = "verifier_gate.rs"]
mod verifier_gate;
