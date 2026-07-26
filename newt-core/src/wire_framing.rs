//! Unambiguous framing for signed and hashed preimages.
//!
//! Every security preimage in the passkey stack is a domain tag followed by a
//! sequence of fields. Concatenating fields raw would let a tamperer move a
//! byte across a boundary and keep the digest — moving one character from the
//! issuer into the subject, say. Length-prefixing each field makes the split
//! points part of what is signed, so only one field sequence can produce a
//! given preimage.
//!
//! Ports must match this exactly: an 8-byte big-endian length, then the field
//! bytes, with no separator and no padding.

/// Append `field` to `out`, prefixed by its big-endian `u64` length.
pub(crate) fn push_field(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u64).to_be_bytes());
    out.extend_from_slice(field);
}
