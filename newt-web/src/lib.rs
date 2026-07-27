//! Library face of the newt-web cockpit.
//!
//! The binary keeps its own route modules; this exists so the security-bearing
//! pieces are importable — by the binary, and by suites in `tests/` that would
//! otherwise have to live inside the source files they exercise.

pub mod webauthn;
