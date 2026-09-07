//! Test families for [`super`] (`slash_registry`), split out of
//! `slash_registry.rs` by what each test asserts. Sibling files; no
//! behaviour change.
//!
//! What did NOT move, and why: every test whose haystack arrives through a
//! bare-sibling `include_str!` stays in `slash_registry.rs`, because that
//! macro resolves against the file that contains it. See the PR body.

// A glob RE-EXPORT, not a plain glob: the moved bodies write `super::X` from
// when `super` was `slash_registry`, and a private glob binding is not
// nameable by path from a child module. It also carries the three hoisted
// `#[cfg(test)]` scan helpers that stayed behind with their `include_str!`.
pub(crate) use super::*;

fn contains_dispatch_token(src: &str, token: &str) -> bool {
    src.contains(&format!("\"{token}\"")) || src.contains(&format!(".strip_prefix(\"{token} \")"))
}

#[cfg(test)]
#[path = "dispatch_containment.rs"]
mod dispatch_containment;
#[cfg(test)]
#[path = "fallthrough.rs"]
mod fallthrough;
#[cfg(test)]
#[path = "receipts.rs"]
mod receipts;
#[cfg(test)]
#[path = "retirement.rs"]
mod retirement;
#[cfg(test)]
#[path = "settings_absorption.rs"]
mod settings_absorption;
#[cfg(test)]
#[path = "surface_ratchet.rs"]
mod surface_ratchet;
#[cfg(test)]
#[path = "token_lookup.rs"]
mod token_lookup;
