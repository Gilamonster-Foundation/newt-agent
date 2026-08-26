//! **Newt Markup v1 — the envelope grammar (A1 of epic #1803, #1825).**
//!
//! ADR: `docs/decisions/newt_markup_interaction_architecture.md`. Newt Markup
//! is a progressive-enhancement superset of GFM Markdown: an optional
//! `+++`-fenced TOML front-matter ENVELOPE at the very start of a document,
//! followed by an ordinary Markdown body. Stripping the envelope always
//! leaves the body byte-identical — law 1 of the ADR.
//!
//! This module owns only the ENVELOPE grammar: where a document's typed
//! metadata ends and its Markdown body begins. It parses no TOML and no
//! Markdown (consumers do), takes no rendering dependency, and is compiled
//! unconditionally — the wyvern/lean tier splits documents too. The
//! canonical Markdown dialect lives in [`dialect`] behind the `markdown`
//! feature, beside the parsers that consume it.
//!
//! ## Grammar (v1, deliberately prefix-only)
//!
//! - Input is UTF-8 (`&str`); an optional leading U+FEFF BOM is tolerated
//!   and treated as part of the envelope span when a fence follows it.
//! - The OPENING fence is the exact bytes `+++` starting the document (after
//!   the optional BOM), terminated by `\n`, `\r\n`, or end of input. Any
//!   other first line — including a leading blank line, or `+++foo` — means
//!   the document has NO envelope and is passed through byte-identically.
//! - The CLOSING fence is the first subsequent line whose content, after
//!   trimming the trailing newline family and surrounding whitespace, is
//!   exactly `+++`. (Asymmetric with the opening fence on purpose: this is
//!   the behavior the role-profile corpus shipped with, frozen by the A0
//!   inventory.)
//! - The BODY is everything after the closing-fence line, byte-identical —
//!   no trimming, no line normalization. `\r\n` line endings inside front
//!   matter or body are the author's business.
//! - The grammar is PREFIX-ONLY: metadata-like text anywhere past the first
//!   line — in a fenced code block, a blockquote, or plain prose — can never
//!   activate. There is deliberately no scan of the body.
//!
//! ## Malformed-envelope limits (fail closed)
//!
//! An opened-but-never-closed fence is an error, not a heuristic recovery —
//! guessing whether ten kilobytes of text "looked like" metadata is exactly
//! the drift law 5 forbids. Front matter larger than
//! [`MAX_ENVELOPE_BYTES`] is likewise malformed: no honest interaction
//! definition needs it, and a bound keeps a hostile document from turning
//! the closing-fence scan into work proportional to an unbounded envelope.
//!
//! ## Idempotence boundary (documented, not hand-waved)
//!
//! [`strip_newt_metadata`] is deterministic, byte-preserving outside the
//! removed envelope span, and idempotent on every ASSEMBLE-VALID document.
//! A body that would itself open an envelope makes a second strip eat into
//! it, so [`assemble_newt_metadata`] refuses such bodies
//! ([`EnvelopeError::BodyOpensAnEnvelope`]) — the lint the epic requires so
//! metadata and fallback cannot drift. The lint asks the real splitter
//! rather than approximating it, so it is exactly as strict as the grammar:
//! no more (a whitespace-padded `  +++  ` line never opens an envelope) and
//! no less (a BOM-prefixed fence does). Splitting an adversarial
//! hand-written document of that shape consumes only the FIRST envelope
//! (see `split_consumes_only_the_first_envelope`).

use std::fmt;

#[cfg(feature = "markdown")]
pub mod dialect;

/// The envelope fence marker. Must open the document on its own line and
/// close on its own line.
pub const FENCE: &str = "+++";

/// Malformed-envelope bound: front matter larger than this is refused. Far
/// above any honest interaction definition (the largest shipped role
/// profile's front matter is under 2 KiB), far below "the parser will chew
/// on anything".
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;

/// How an envelope can be malformed. Fail closed: none of these fall back
/// to guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// An opening `+++` fence with no closing `+++` line.
    Unclosed,
    /// The envelope exceeded [`MAX_ENVELOPE_BYTES`].
    Oversized {
        /// The envelope span measured when the limit was hit — the offset
        /// scanned to, which for a terminated envelope is the front matter's
        /// length and for an unterminated one is the unscanned remainder.
        /// A magnitude for the operator, not an exact front-matter size.
        bytes: usize,
    },
    /// `assemble` only: the body would itself open an envelope under the
    /// grammar's OPENING-fence rule, so a second strip would consume part of
    /// it (or fail). Refused so strip stays idempotent on every assembled
    /// document.
    BodyOpensAnEnvelope,
    /// `assemble` only: the front matter contains a line that would read as
    /// a closing fence, truncating it on the next split.
    FrontMatterContainsFence,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unclosed => {
                write!(f, "front-matter opened with `{FENCE}` but never closed")
            }
            Self::Oversized { bytes } => write!(
                f,
                "front-matter is {bytes} bytes; the envelope limit is {MAX_ENVELOPE_BYTES}"
            ),
            Self::BodyOpensAnEnvelope => write!(
                f,
                "the body itself opens a `{FENCE}` envelope — assembling it would \
                 make stripping non-idempotent"
            ),
            Self::FrontMatterContainsFence => write!(
                f,
                "the front matter contains a `{FENCE}` line, which would read as \
                 its closing fence"
            ),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// A document split at the envelope boundary. Both halves borrow the input;
/// nothing is copied or normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDocument<'a> {
    /// The text between the fences (exclusive), when an envelope is present.
    pub front_matter: Option<&'a str>,
    /// Everything after the closing-fence line — or the whole input,
    /// byte-identical (BOM and all), when there is no envelope.
    pub body: &'a str,
}

/// Split a document at the envelope boundary.
///
/// # Errors
///
/// [`EnvelopeError::Unclosed`] when an opening fence never closes;
/// [`EnvelopeError::Oversized`] when the front matter exceeds
/// [`MAX_ENVELOPE_BYTES`].
pub fn split_newt_metadata(text: &str) -> Result<SplitDocument<'_>, EnvelopeError> {
    // The BOM is tolerated only immediately before the opening fence. We
    // intentionally do not skip blank lines: a leading blank line means "no
    // envelope".
    let after_bom = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = after_bom.strip_prefix(FENCE) else {
        return Ok(SplitDocument {
            front_matter: None,
            body: text,
        });
    };
    // The opening fence must be its own line: the bytes right after `+++`
    // must be a newline (or the input ends — an opened, unclosed envelope).
    let rest = match rest.strip_prefix('\n') {
        Some(r) => r,
        None => match rest.strip_prefix("\r\n") {
            Some(r) => r,
            None if rest.is_empty() => "",
            // `+++foo` on the first line is not a fence — the whole input is
            // the body.
            None => {
                return Ok(SplitDocument {
                    front_matter: None,
                    body: text,
                })
            }
        },
    };
    // Find the closing fence: the first line that is exactly `+++` after
    // whitespace trimming (frozen role-profile behavior).
    for (idx, line) in LineOffsets::new(rest) {
        if idx > MAX_ENVELOPE_BYTES {
            return Err(EnvelopeError::Oversized { bytes: idx });
        }
        if line.trim_end_matches(['\r', '\n']).trim() == FENCE {
            // `idx` IS the front matter's length here, and the check above
            // already bounded it — the load-bearing guard is that one.
            debug_assert!(idx <= MAX_ENVELOPE_BYTES);
            let front_matter = &rest[..idx];
            return Ok(SplitDocument {
                front_matter: Some(front_matter),
                body: &rest[idx + line.len()..],
            });
        }
    }
    // The whole remainder was scanned without a closing fence. When that
    // remainder itself exceeds the envelope bound, Oversized is the truer
    // verdict than Unclosed — the document demanded an over-limit scan.
    if rest.len() > MAX_ENVELOPE_BYTES {
        return Err(EnvelopeError::Oversized { bytes: rest.len() });
    }
    Err(EnvelopeError::Unclosed)
}

/// Remove the envelope, returning the body as a borrowed slice of the input
/// — deterministic, byte-preserving outside the removed span, and
/// idempotent on assemble-valid documents (see the module doc's idempotence
/// boundary). A document with no envelope comes back byte-identical.
///
/// # Errors
///
/// Exactly [`split_newt_metadata`]'s: a malformed envelope fails closed
/// rather than guessing at a body.
pub fn strip_newt_metadata(text: &str) -> Result<&str, EnvelopeError> {
    Ok(split_newt_metadata(text)?.body)
}

/// Serialize an (front matter, body) pair into envelope form such that
/// [`split_newt_metadata`] returns it verbatim (the round-trip property).
/// Front matter is given without fences; a missing trailing newline is
/// supplied so the closing fence sits on its own line.
///
/// # Errors
///
/// [`EnvelopeError::FrontMatterContainsFence`] when the front matter holds a
/// line that would read as the closing fence (the split would truncate it);
/// [`EnvelopeError::Oversized`] when it exceeds [`MAX_ENVELOPE_BYTES`];
/// [`EnvelopeError::BodyStartsWithFence`] when the body's first line is a
/// fence (strip idempotence — the lint the ADR requires of generated and
/// hand-authored documents alike).
pub fn assemble_newt_metadata(front_matter: &str, body: &str) -> Result<String, EnvelopeError> {
    let newline = if front_matter.is_empty() || front_matter.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    // Bound the TERMINATED span — the bytes `split` will actually scan.
    // Bounding the unterminated text instead would accept a front matter
    // that becomes one byte over once the closing newline is supplied, and
    // the document we just built would not split back.
    let span = front_matter.len() + newline.len();
    if span > MAX_ENVELOPE_BYTES {
        return Err(EnvelopeError::Oversized { bytes: span });
    }
    if front_matter
        .lines()
        .any(|l| l.trim_end_matches('\r').trim() == FENCE)
    {
        return Err(EnvelopeError::FrontMatterContainsFence);
    }
    // The body must be envelope-free under the SAME rule `split` applies.
    // Testing it with the real splitter rather than a hand-written "starts
    // with a fence" approximation is what makes the idempotence claim true:
    // an approximation both over-refuses harmless lookalikes (`  +++  `,
    // which the opening rule never honors) and — worse — misses shapes it
    // does honor, such as a BOM-prefixed fence, which `.trim()` does not
    // strip because U+FEFF is a format character, not whitespace.
    if !matches!(
        split_newt_metadata(body),
        Ok(SplitDocument {
            front_matter: None,
            ..
        })
    ) {
        return Err(EnvelopeError::BodyOpensAnEnvelope);
    }
    Ok(format!("{FENCE}\n{front_matter}{newline}{FENCE}\n{body}"))
}

/// Iterator over `(byte_offset, line_with_terminator)` pairs of a string.
/// (Moved verbatim from `role_profile.rs` with the splitter it serves.)
struct LineOffsets<'a> {
    rest: &'a str,
    offset: usize,
}

impl<'a> LineOffsets<'a> {
    fn new(s: &'a str) -> Self {
        Self { rest: s, offset: 0 }
    }
}

impl<'a> Iterator for LineOffsets<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let end = match self.rest.find('\n') {
            Some(i) => i + 1,
            None => self.rest.len(),
        };
        let line = &self.rest[..end];
        let start = self.offset;
        self.offset += end;
        self.rest = &self.rest[end..];
        Some((start, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_front_matter_and_body() {
        let fm = "role = \"navigator\"\ntier = 2\n";
        let body = "# Prompt\n\nDo the thing.\n";
        let doc = assemble_newt_metadata(fm, body).unwrap();
        let split = split_newt_metadata(&doc).unwrap();
        assert_eq!(split.front_matter, Some(fm));
        assert_eq!(split.body, body);
        // A missing trailing newline on the front matter is supplied, and
        // the round trip then reproduces the newline-terminated form.
        let doc2 = assemble_newt_metadata("a = 1", body).unwrap();
        assert_eq!(
            split_newt_metadata(&doc2).unwrap().front_matter,
            Some("a = 1\n")
        );
    }

    #[test]
    fn strip_is_idempotent_on_valid_documents() {
        let doc = assemble_newt_metadata("a = 1\n", "body text\nmore\n").unwrap();
        let once = strip_newt_metadata(&doc).unwrap();
        let twice = strip_newt_metadata(once).unwrap();
        assert_eq!(once, "body text\nmore\n");
        assert_eq!(once, twice, "stripping a stripped document is a no-op");
        // No envelope at all: byte-identical passthrough, also idempotent.
        let plain = "just markdown\n\n+++ not at the start\n";
        assert_eq!(strip_newt_metadata(plain).unwrap(), plain);
    }

    #[test]
    fn strip_is_byte_preserving_outside_the_envelope() {
        // Trailing whitespace, CRLF line endings, and a non-trimmed body
        // survive byte-for-byte — the grammar never normalizes the body.
        let body = "  leading spaces kept\r\ntrailing too  \n\n";
        let doc = format!("+++\na = 1\n+++\n{body}");
        assert_eq!(strip_newt_metadata(&doc).unwrap(), body);
    }

    #[test]
    fn body_metadata_lookalikes_never_activate() {
        // Prefix-only grammar: fences past the first line are content.
        for body in [
            "text\n+++\nrole = \"x\"\n+++\n",
            "```\n+++\ninside a code fence\n+++\n```\n",
            "> +++\n> quoted\n> +++\n",
        ] {
            assert_eq!(
                strip_newt_metadata(body).unwrap(),
                body,
                "a mid-document fence must never activate: {body:?}"
            );
        }
    }

    /// Named in #1825: a leading blank line means the document has no
    /// envelope — frozen role-profile behavior, deliberately not "skip
    /// whitespace then look for a fence".
    #[test]
    fn a_leading_blank_line_means_no_front_matter() {
        for doc in [
            "\n+++\na = 1\n+++\nbody\n",
            "\r\n+++\na = 1\n+++\nbody\n",
            " +++\na = 1\n+++\nbody\n",
        ] {
            let split = split_newt_metadata(doc).unwrap();
            assert_eq!(split.front_matter, None, "no envelope for {doc:?}");
            assert_eq!(split.body, doc, "byte-identical passthrough for {doc:?}");
        }
    }

    /// Named in #1825: the idempotence lint, stated as its own contract.
    /// It is exactly as strict as the OPENING-fence rule — including the
    /// BOM-prefixed shape a `starts_with("+++")` approximation would miss,
    /// and excluding the padded lookalikes such an approximation would
    /// wrongly refuse.
    #[test]
    fn assemble_refuses_a_fence_leading_body() {
        for body in [
            "+++\nb = 2\n+++\nmore\n",
            "+++\r\nb = 2\r\n+++\r\nmore\r\n",
            // BOM-prefixed: the opening rule tolerates the BOM, so a second
            // strip WOULD eat this body. (Regression: a trim-based lint
            // passed this through — U+FEFF is a format char, not whitespace.)
            "\u{feff}+++\nb = 2\n+++\nmore\n",
            // Opens an envelope that never closes: a second strip would not
            // eat the body, it would ERROR — equally non-idempotent.
            "+++\nb = 2\nnever closed\n",
        ] {
            assert_eq!(
                assemble_newt_metadata("a = 1\n", body),
                Err(EnvelopeError::BodyOpensAnEnvelope),
                "must refuse an envelope-opening body: {body:?}"
            );
        }
        // ...and only those: a padded or suffixed lookalike never opens an
        // envelope, so it is a perfectly good body.
        for body in ["  +++  \nrest\n", "+++ \nrest\n", "+++toml\nrest\n"] {
            let doc = assemble_newt_metadata("a = 1\n", body)
                .unwrap_or_else(|e| panic!("lookalike body must be accepted ({body:?}): {e}"));
            assert_eq!(strip_newt_metadata(&doc).unwrap(), body);
            assert_eq!(
                strip_newt_metadata(body).unwrap(),
                body,
                "and stripping it again is a no-op — the lint's whole point"
            );
        }
    }

    /// Every assembled document round-trips AT the size boundary, with or
    /// without the author's trailing newline. (Regression: bounding the
    /// unterminated front matter accepted a document one byte over the
    /// limit once the closing newline was supplied, which then refused to
    /// split — an accepted input violating the round-trip law.)
    #[test]
    fn the_envelope_bound_is_the_terminated_span() {
        let exact_unterminated = "x".repeat(MAX_ENVELOPE_BYTES);
        assert!(
            matches!(
                assemble_newt_metadata(&exact_unterminated, "body\n"),
                Err(EnvelopeError::Oversized { bytes }) if bytes == MAX_ENVELOPE_BYTES + 1
            ),
            "a newline must be supplied, so the terminated span is over the limit"
        );
        // One byte smaller: the supplied newline lands exactly on the bound,
        // and the assembled document splits back verbatim.
        let fits = "x".repeat(MAX_ENVELOPE_BYTES - 1);
        let doc = assemble_newt_metadata(&fits, "body\n").unwrap();
        let split = split_newt_metadata(&doc).unwrap();
        assert_eq!(split.front_matter.unwrap().len(), MAX_ENVELOPE_BYTES);
        assert_eq!(split.body, "body\n");
        // Already newline-terminated at exactly the bound: no supplement, so
        // it fits and round-trips.
        let exact_terminated = format!("{}\n", "x".repeat(MAX_ENVELOPE_BYTES - 1));
        let doc = assemble_newt_metadata(&exact_terminated, "body\n").unwrap();
        assert_eq!(
            split_newt_metadata(&doc).unwrap().front_matter,
            Some(exact_terminated.as_str())
        );
    }

    /// The adversarial shape the idempotence boundary documents: split
    /// consumes only the FIRST envelope; the fence-leading remainder is the
    /// body, and `assemble` refuses to produce such a document.
    #[test]
    fn split_consumes_only_the_first_envelope() {
        let doc = "+++\na = 1\n+++\n+++\nb = 2\n+++\nreal body\n";
        let split = split_newt_metadata(doc).unwrap();
        assert_eq!(split.front_matter, Some("a = 1\n"));
        assert_eq!(split.body, "+++\nb = 2\n+++\nreal body\n");
        assert_eq!(
            assemble_newt_metadata("a = 1\n", split.body),
            Err(EnvelopeError::BodyOpensAnEnvelope),
            "assemble lints away the shape that would break idempotence"
        );
    }

    #[test]
    fn an_unclosed_fence_fails_closed() {
        assert_eq!(
            split_newt_metadata("+++\na = 1\nno closing fence\n"),
            Err(EnvelopeError::Unclosed)
        );
        // A bare `+++` with nothing after it is an opened, unclosed envelope.
        assert_eq!(split_newt_metadata("+++"), Err(EnvelopeError::Unclosed));
        assert_eq!(split_newt_metadata("+++\n"), Err(EnvelopeError::Unclosed));
    }

    #[test]
    fn an_oversized_envelope_is_malformed() {
        let big = "x = 1\n".repeat(MAX_ENVELOPE_BYTES / 6 + 1);
        let doc = format!("+++\n{big}+++\nbody\n");
        assert!(matches!(
            split_newt_metadata(&doc),
            Err(EnvelopeError::Oversized { .. })
        ));
        // The scan gives up at the bound even when no closing fence exists
        // at all — a hostile unterminated envelope cannot demand an
        // unbounded closing-fence search be classified as merely Unclosed.
        let hostile = format!("+++\n{big}");
        assert!(matches!(
            split_newt_metadata(&hostile),
            Err(EnvelopeError::Oversized { .. })
        ));
        assert!(matches!(
            assemble_newt_metadata(&big, "body\n"),
            Err(EnvelopeError::Oversized { .. })
        ));
        // Front matter at exactly the bound (already newline-terminated) is
        // accepted — the limit is inclusive, and `assemble`/`split` agree on
        // which side of it a document falls.
        let at_bound = format!("{}\n", "x".repeat(MAX_ENVELOPE_BYTES - 1));
        assert!(assemble_newt_metadata(&at_bound, "body\n").is_ok());
    }

    #[test]
    fn bom_and_crlf_fences_still_split() {
        let doc = "\u{feff}+++\r\na = 1\r\n+++\r\nbody\r\n";
        let split = split_newt_metadata(doc).unwrap();
        assert_eq!(split.front_matter, Some("a = 1\r\n"));
        assert_eq!(split.body, "body\r\n");
        // Without a fence the BOM is body like everything else.
        let plain = "\u{feff}no fence\n";
        assert_eq!(strip_newt_metadata(plain).unwrap(), plain);
    }

    #[test]
    fn a_fence_with_trailing_garbage_is_body_not_envelope() {
        let doc = "+++toml\na = 1\n+++\nbody\n";
        let split = split_newt_metadata(doc).unwrap();
        assert_eq!(split.front_matter, None);
        assert_eq!(split.body, doc);
    }

    #[test]
    fn assemble_refuses_a_fence_bearing_front_matter() {
        assert_eq!(
            assemble_newt_metadata("a = 1\n+++\nb = 2\n", "body\n"),
            Err(EnvelopeError::FrontMatterContainsFence)
        );
        // Whitespace-padded closing-fence lookalikes are refused too — the
        // split's closing scan trims, so assemble must lint what split sees.
        assert_eq!(
            assemble_newt_metadata("a = 1\n  +++  \n", "body\n"),
            Err(EnvelopeError::FrontMatterContainsFence)
        );
    }

    #[test]
    fn errors_render_actionable_text() {
        assert_eq!(
            EnvelopeError::Unclosed.to_string(),
            "front-matter opened with `+++` but never closed"
        );
        assert!(EnvelopeError::Oversized { bytes: 70_000 }
            .to_string()
            .contains("65536"));
    }
}
