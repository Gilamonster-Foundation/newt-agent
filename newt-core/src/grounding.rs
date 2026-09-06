//! The grounding kernel (#1756) — is this text actually derived from that source?
//!
//! The third of the harness's claim checks, and the one with exact ground truth:
//!
//! | module | asks |
//! |---|---|
//! | [`crate::verify_gate`] (#73) | do the code's imports resolve against the real surface? |
//! | [`crate::agentic::claim_check`] (#867) | do the prose's file paths exist in the workspace? |
//! | **this** | do the output's words exist in the source it claims to be a conversion of? |
//!
//! # Why an extractive task can be gated and a generative one cannot
//!
//! Reformatting, converting, transcribing, and quoting are *extractive*: nearly every
//! content word in the output must already appear in the input. So "did the model invent
//! this?" is answerable by string processing — no judge model, no second inference, cheap
//! enough to run on every chunk of a long conversion. That is what makes this a **gate**
//! rather than a report.
//!
//! The failure it exists for (#1755): handed a 1.6 MB browser capture whose readable body
//! never reached the model, the harness let a confident, well-structured Markdown
//! "conversion" through in which every asserted entity was fabricated and every source
//! fact was absent. Measured here, that document scores as one enormous ungrounded
//! passage in microseconds.
//!
//! # Spans, not scores
//!
//! [`check`] returns the ungrounded material as **maximal runs with byte offsets into
//! the output** — byte, not character, so a span is directly `&output[start..end]`;
//! because the caller's job is to act: annotate the span, re-ask for that window, or
//! mark it unconverted. A bare rate cannot be pointed at.
//!
//! # Normalization is not cosmetic
//!
//! Both sides are lower-cased, digit-group-joined, split on non-alphanumerics, and — the
//! load-bearing step — **soft-line-break-unwrapped**. A quoted-printable capture splits
//! words mid-token (`Zawadow=\nski`), so without unwrapping, a *correct* conversion reads
//! as fabricated and the gate would fire on the harness's own encoding rather than on the
//! model's invention.
//!
//! # Independence from the benchmark (deliberate — do not consolidate)
//!
//! `gilamonster-bench` scores the same properties from its own published spec
//! (`FIDELITY.md`) with its own implementation, and must not depend on this module. If the
//! ruler and the gate shared code, a bug here would pass the gate and score as passing
//! *because they are the same code*. Divergence between the two is a signal worth being
//! able to see.
//!
//! Pure by construction: no filesystem, no network, no model.

use std::collections::{HashMap, HashSet};

/// Default n-gram width. Wide enough that ordinary shared phrasing ("of the", "and then
/// he") cannot register as novel; narrow enough to catch a fabricated sentence.
// INERT-CODE-RATCHET: X19 DELETE: grounding API is a closed tested island with no production entry point.
pub const DEFAULT_NGRAM: usize = 5;

/// A quoted span shorter than this carries no evidence either way, and scoring it would
/// punish ordinary emphasis. Precision over recall, like the sibling claim checks.
pub const MIN_QUOTE_TOKENS: usize = 3;

/// Knobs. Defaults are the conversion profile.
#[derive(Debug, Clone)]
pub struct GroundingConfig {
    pub ngram: usize,
    pub min_quote_tokens: usize,
}

impl Default for GroundingConfig {
    fn default() -> Self {
        Self {
            ngram: DEFAULT_NGRAM,
            min_quote_tokens: MIN_QUOTE_TOKENS,
        }
    }
}

/// A stretch of output the source does not support, addressed so a caller can act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Byte offsets into the ORIGINAL output string, so the caller can slice, highlight,
    /// or replace exactly this text.
    pub start: usize,
    pub end: usize,
    /// The offending text as it appears in the output.
    pub text: String,
}

/// What kind of thing was not grounded. Three kinds, because a caller treats them
/// differently: a passage may be re-asked, a figure is a hard stop, a quotation is a
/// fabricated attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ungrounded {
    /// A run of content words with no counterpart in the source — invented prose.
    Passage(Span),
    /// A numeral the source does not contain. Numbers are where invention surfaces
    /// first and they are checkable without any judgement at all.
    Number(Span),
    /// Text inside quotation marks that is not verbatim in the source. Quotation marks
    /// are a claim of exact reproduction; a paraphrase inside them is a fabrication
    /// however faithful the sense.
    Quotation(Span),
}

impl Ungrounded {
    #[must_use]
    pub fn span(&self) -> &Span {
        match self {
            Self::Passage(s) | Self::Number(s) | Self::Quotation(s) => s,
        }
    }

    /// Model-facing label, for the annotation or the retry instruction.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Passage(_) => "not in the source document",
            Self::Number(_) => "a figure the source document does not contain",
            Self::Quotation(_) => "quoted, but not verbatim in the source document",
        }
    }
}

/// The verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct Grounding {
    /// Everything the source does not support, in output order.
    pub ungrounded: Vec<Ungrounded>,
    /// Fraction of the output's content n-grams that ARE grounded. 1.0 for a faithful
    /// conversion; reported for telemetry, never as the gate's decision — the decision is
    /// whether `ungrounded` is empty.
    pub grounded_ratio: f64,
    /// Fraction of the source's paragraphs represented in the output. NOT a fidelity
    /// measure: a partial conversion is legitimate work, and on a small context window it
    /// is the expected outcome. It is the honesty input — see [`Grounding::overclaims`].
    pub source_coverage: f64,
}

impl Grounding {
    /// Nothing invented. The gate's pass condition.
    #[must_use]
    pub fn is_grounded(&self) -> bool {
        self.ungrounded.is_empty()
    }

    /// Did the turn present partial work as finished? Partial conversion is fine;
    /// partial conversion reported as complete is the failure that reaches a user, and it
    /// is the one the motivating document committed — it was confident, complete-looking,
    /// and carried no signal that its source had never been read.
    #[must_use]
    pub fn overclaims(&self, claimed_complete: bool, coverage_floor: f64) -> bool {
        claimed_complete && self.source_coverage < coverage_floor
    }

    /// One line per problem, for an appended annotation or a retry instruction. The
    /// model's own prose is never rewritten — the same discipline
    /// [`crate::agentic::claim_check`] follows.
    #[must_use]
    pub fn annotations(&self) -> Vec<String> {
        self.ungrounded
            .iter()
            .map(|u| format!("{:?} — {}", u.span().text, u.label()))
            .collect()
    }
}

/// Check `output` against `source`.
#[must_use]
pub fn check(source: &str, output: &str, cfg: &GroundingConfig) -> Grounding {
    let n = cfg.ngram.max(1);
    let index = SourceIndex::build(source, n);
    let out_tokens = tokenize(output);

    let covered = grounded_token_mask(&out_tokens, &index, n);
    let grounded_ratio = if out_tokens.is_empty() {
        1.0
    } else {
        covered.iter().filter(|c| **c).count() as f64 / out_tokens.len() as f64
    };

    let mut ungrounded: Vec<Ungrounded> = Vec::new();
    for span in uncovered_runs(output, &out_tokens, &covered) {
        ungrounded.push(Ungrounded::Passage(span));
    }
    for token in out_tokens.iter().filter(|t| t.is_numeral()) {
        if !index.tokens.contains(&token.text) {
            ungrounded.push(Ungrounded::Number(token.span(output)));
        }
    }
    for (span, tokens) in quoted_spans(output, cfg.min_quote_tokens) {
        if !index.contains_run(&tokens) {
            ungrounded.push(Ungrounded::Quotation(span));
        }
    }
    ungrounded.sort_by_key(|u| u.span().start);

    Grounding {
        ungrounded,
        grounded_ratio,
        source_coverage: index.coverage_of(&out_tokens, n),
    }
}

/// A normalized token plus where it came from in the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: String,
    start: usize,
    end: usize,
}

impl Token {
    fn is_numeral(&self) -> bool {
        self.text.chars().all(|c| c.is_ascii_digit())
    }

    fn span(&self, original: &str) -> Span {
        Span {
            start: self.start,
            end: self.end,
            text: original[self.start..self.end].to_string(),
        }
    }
}

/// Normalize while keeping byte offsets: unwrap quoted-printable soft breaks, fold case,
/// drop digit-group commas, split on non-alphanumerics.
///
/// Soft breaks are handled WITHOUT rewriting the string (which would invalidate every
/// offset): a `=` immediately followed by a newline is skipped, and the token continues
/// across it, so `Zawadow=\nski` yields one token spanning the break.
fn tokenize(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < text.len() {
        // Quoted-printable soft break: `=` + CRLF/LF continues the current token.
        // The width is measured, never assumed: a bare `=\r` is two bytes, and
        // skipping three would step into the FOLLOWING character — eating an
        // ASCII one silently (`ab=\rcd` tokenized as `abd`) and slicing a
        // multibyte one mid-scalar, which panics `text[i..]` on the next
        // iteration. Both are reachable from any CR-bearing capture.
        if bytes[i] == b'=' {
            let soft = match (bytes.get(i + 1), bytes.get(i + 2)) {
                (Some(b'\r'), Some(b'\n')) => 3,
                (Some(b'\r'), _) | (Some(b'\n'), _) => 2,
                _ => 0,
            };
            if soft > 0 {
                i += soft;
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("char boundary");
        let width = ch.len_utf8();

        // A comma between two digits is a group separator, not a boundary.
        let group_comma = ch == ','
            && !current.is_empty()
            && current.ends_with(|c: char| c.is_ascii_digit())
            && text[i + width..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_digit());
        if group_comma {
            i += width;
            continue;
        }

        if ch.is_alphanumeric() {
            if current.is_empty() {
                start = i;
            }
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(Token {
                text: std::mem::take(&mut current),
                start,
                end: i,
            });
        }
        i += width;
    }
    if !current.is_empty() {
        tokens.push(Token {
            text: current,
            start,
            end: text.len(),
        });
    }
    tokens
}

/// The source, indexed once: hashed n-grams for the grounding walk, a token-position map
/// for arbitrary-length runs (quotations), the token set for figures, and the paragraph
/// split for coverage.
struct SourceIndex {
    words: Vec<String>,
    positions: HashMap<String, Vec<usize>>,
    tokens: HashSet<String>,
    grams: HashSet<u64>,
    paragraphs: Vec<Vec<String>>,
}

impl SourceIndex {
    fn build(source: &str, n: usize) -> Self {
        let words: Vec<String> = tokenize(source).into_iter().map(|t| t.text).collect();
        let mut positions: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, w) in words.iter().enumerate() {
            positions.entry(w.clone()).or_default().push(i);
        }
        let grams = window_hashes(&words, n).into_iter().collect();
        let tokens = words.iter().cloned().collect();
        let paragraphs = source
            .split("\n\n")
            .map(|p| tokenize(p).into_iter().map(|t| t.text).collect::<Vec<_>>())
            .filter(|p: &Vec<String>| !p.is_empty())
            .collect();
        Self {
            words,
            positions,
            tokens,
            grams,
            paragraphs,
        }
    }

    /// Contiguous run lookup, seeking from the first token's positions — a long source
    /// costs occurrences, not length.
    fn contains_run(&self, run: &[String]) -> bool {
        if run.is_empty() {
            return true;
        }
        let Some(starts) = self.positions.get(&run[0]) else {
            return false;
        };
        starts.iter().any(|&p| {
            self.words
                .get(p..p + run.len())
                .is_some_and(|window| window == run)
        })
    }

    /// Fraction of source paragraphs reaching the output. A paragraph shorter than `n`
    /// (a heading, a caption) is matched as a contiguous run rather than dropped from the
    /// denominator — dropping it would inflate coverage on documents that are mostly
    /// headings.
    fn coverage_of(&self, out_tokens: &[Token], n: usize) -> f64 {
        if self.paragraphs.is_empty() {
            return 0.0;
        }
        let words: Vec<String> = out_tokens.iter().map(|t| t.text.clone()).collect();
        let out_grams: HashSet<u64> = window_hashes(&words, n).into_iter().collect();
        let hit = self
            .paragraphs
            .iter()
            .filter(|p| {
                if p.len() >= n {
                    window_hashes(p, n).iter().any(|g| out_grams.contains(g))
                } else {
                    words.windows(p.len()).any(|w| w == p.as_slice())
                }
            })
            .count();
        hit as f64 / self.paragraphs.len() as f64
    }
}

/// Which output tokens are supported by the source: a token is grounded when ANY n-gram
/// containing it occurs in the source. Token-level (rather than gram-level) because the
/// caller needs contiguous runs to point at, and a gram list cannot be turned back into
/// one without this step.
fn grounded_token_mask(out_tokens: &[Token], index: &SourceIndex, n: usize) -> Vec<bool> {
    let words: Vec<String> = out_tokens.iter().map(|t| t.text.clone()).collect();
    let mut covered = vec![false; words.len()];

    if words.len() < n {
        // Too short for a window: fall back to whole-run containment, so a one-line
        // output is judged rather than exempted.
        let grounded = index.contains_run(&words);
        return vec![grounded; words.len()];
    }
    for (i, hash) in window_hashes(&words, n).into_iter().enumerate() {
        if index.grams.contains(&hash) {
            covered[i..i + n].iter_mut().for_each(|c| *c = true);
        }
    }
    covered
}

/// Maximal runs of ungrounded tokens, as spans of the ORIGINAL output text (so the run
/// reads as written, punctuation and all, when it is shown back to the model).
fn uncovered_runs(original: &str, tokens: &[Token], covered: &[bool]) -> Vec<Span> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if covered[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < tokens.len() && !covered[i] {
            i += 1;
        }
        let from = tokens[start].start;
        let to = tokens[i - 1].end;
        runs.push(Span {
            start: from,
            end: to,
            text: original[from..to].to_string(),
        });
    }
    runs
}

/// Quoted spans (straight and typographic) with at least `min_tokens` tokens, paired with
/// their normalized token run. An unpaired mark yields nothing rather than swallowing the
/// rest of the document.
fn quoted_spans(text: &str, min_tokens: usize) -> Vec<(Span, Vec<String>)> {
    let mut out = Vec::new();
    for (open, close) in [('"', '"'), ('\u{201C}', '\u{201D}')] {
        let mut start: Option<usize> = None;
        for (i, c) in text.char_indices() {
            match start {
                None if c == open => start = Some(i + c.len_utf8()),
                Some(s) if c == close => {
                    let inner = &text[s..i];
                    let run: Vec<String> = tokenize(inner).into_iter().map(|t| t.text).collect();
                    if run.len() >= min_tokens {
                        out.push((
                            Span {
                                start: s,
                                end: i,
                                text: inner.to_string(),
                            },
                            run,
                        ));
                    }
                    start = None;
                }
                _ => {}
            }
        }
    }
    out.sort_by_key(|(span, _)| span.start);
    out
}

/// FNV-1a hashes of every contiguous n-token window. Hashing rather than storing the
/// joined strings keeps a book-sized source cheap to index.
fn window_hashes(words: &[String], n: usize) -> Vec<u64> {
    if words.len() < n {
        return Vec::new();
    }
    words
        .windows(n)
        .map(|w| {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for (i, word) in w.iter().enumerate() {
                if i > 0 {
                    hash ^= u64::from(b' ');
                    hash = hash.wrapping_mul(0x1000_0000_01b3);
                }
                for b in word.bytes() {
                    hash ^= u64::from(b);
                    hash = hash.wrapping_mul(0x1000_0000_01b3);
                }
            }
            hash
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "\
Two mathematicians spun a coin 250 times and found it landed heads up 140 times.

The newsroom repeated the experiment and recorded 139 heads to 111 tails.

A statistician said an unbiased coin would produce a result that extreme \
\u{201C}less than seven percent of the time\u{201D} in ordinary conditions.";

    fn check_default(source: &str, output: &str) -> Grounding {
        check(source, output, &GroundingConfig::default())
    }

    #[test]
    fn a_text_is_grounded_in_itself() {
        let g = check_default(SOURCE, SOURCE);
        assert!(g.is_grounded(), "{:?}", g.annotations());
        assert_eq!(g.grounded_ratio, 1.0);
        assert_eq!(g.source_coverage, 1.0);
    }

    #[test]
    fn markdown_syntax_is_not_content() {
        // The whole premise of gating a REFORMAT: headings, bold, and list syntax must
        // not register as new material, or every correct conversion would trip the gate.
        let reformatted = format!("## Findings\n\n- **{}**\n", SOURCE.replace('\n', " "));
        let g = check_default(SOURCE, &reformatted);
        let passages: Vec<&Ungrounded> = g
            .ungrounded
            .iter()
            .filter(|u| matches!(u, Ungrounded::Passage(_)))
            .collect();
        assert_eq!(
            passages.len(),
            1,
            "only the invented heading is novel: {:?}",
            g.annotations()
        );
        assert!(passages[0].span().text.contains("Findings"));
    }

    #[test]
    fn an_invented_document_is_one_pointable_span() {
        // The #1755 shape. The gate's product is a SPAN the caller can act on, not a rate.
        let invented = "The final finished 3-2 after extra time, with two goals from Rivera.";
        let g = check_default(SOURCE, invented);
        assert!(!g.is_grounded());
        let passage = g
            .ungrounded
            .iter()
            .find_map(|u| match u {
                Ungrounded::Passage(s) => Some(s),
                _ => None,
            })
            .expect("an invented document is an ungrounded passage");
        assert_eq!(
            &invented[passage.start..passage.end],
            passage.text,
            "the span addresses the real output text"
        );
        assert!(passage.text.contains("Rivera"));
    }

    #[test]
    fn a_soft_wrapped_source_still_grounds_its_correct_conversion() {
        // Quoted-printable splits words mid-token. Without unwrapping, the gate would fire
        // on the harness's own encoding rather than on the model's invention.
        let encoded =
            "Two mathematicians spun a coin 250 times and found it landed heads up 140=\n times.";
        let converted =
            "Two mathematicians spun a coin 250 times and found it landed heads up 140 times.";
        let g = check_default(encoded, converted);
        assert!(g.is_grounded(), "{:?}", g.annotations());
    }

    #[test]
    fn an_invented_figure_is_caught_in_otherwise_faithful_prose() {
        let nearly = SOURCE.replace("139 heads", "162 heads");
        let g = check_default(SOURCE, &nearly);
        let numbers: Vec<&Span> = g
            .ungrounded
            .iter()
            .filter_map(|u| match u {
                Ungrounded::Number(s) => Some(s),
                _ => None,
            })
            .collect();
        assert!(
            numbers.iter().any(|s| s.text == "162"),
            "{:?}",
            g.annotations()
        );
    }

    #[test]
    fn a_reworded_quotation_fails_even_when_the_meaning_survives() {
        let reworded = SOURCE.replace("less than seven percent", "under seven percent");
        let g = check_default(SOURCE, &reworded);
        assert!(g
            .ungrounded
            .iter()
            .any(|u| matches!(u, Ungrounded::Quotation(_))));
    }

    #[test]
    fn a_faithful_excerpt_is_grounded_but_not_complete() {
        // Partial conversion is legitimate work — the gate must not punish it. What it
        // reports is the coverage, so the caller can decide whether "done" was honest.
        let excerpt =
            "Two mathematicians spun a coin 250 times and found it landed heads up 140 times.";
        let g = check_default(SOURCE, excerpt);
        assert!(g.is_grounded(), "excerpting is not inventing");
        assert!(g.source_coverage < 0.5, "{}", g.source_coverage);
        assert!(
            !g.overclaims(false, 0.95),
            "an honest partial does not overclaim"
        );
        assert!(
            g.overclaims(true, 0.95),
            "the same output called finished does"
        );
    }

    #[test]
    fn an_empty_output_invents_nothing() {
        let g = check_default(SOURCE, "");
        assert!(g.is_grounded());
        assert_eq!(g.source_coverage, 0.0);
        assert!(
            g.overclaims(true, 0.95),
            "...but calling it a conversion is a lie"
        );
    }

    #[test]
    fn a_short_output_is_judged_rather_than_exempted() {
        // Under the n-gram width there is no window to check, so the whole run is matched
        // instead — otherwise a one-line fabrication would pass by being brief.
        let g = check_default(SOURCE, "Rivera scored");
        assert!(!g.is_grounded(), "{:?}", g.annotations());
        let g = check_default(SOURCE, "the newsroom repeated");
        assert!(g.is_grounded(), "{:?}", g.annotations());
    }

    #[test]
    fn tokenize_keeps_offsets_that_slice_the_original() {
        let text = "## **Chapter 1** — Loomings";
        for token in tokenize(text) {
            let slice = &text[token.start..token.end];
            assert_eq!(
                slice.to_lowercase(),
                token.text,
                "offsets must address the token they describe"
            );
        }
    }

    #[test]
    fn digit_groups_join_and_list_commas_do_not() {
        let words: Vec<String> = tokenize("1,600 items: a, b")
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert_eq!(words, vec!["1600", "items", "a", "b"]);
    }

    #[test]
    fn an_unpaired_quote_mark_yields_nothing() {
        assert!(quoted_spans("an \"unclosed span of text here", 3).is_empty());
        assert!(
            quoted_spans("\"two words\"", 3).is_empty(),
            "under the token floor"
        );
    }

    #[test]
    fn annotations_name_the_text_and_the_reason() {
        let g = check_default(SOURCE, "The manager praised his squad after the win.");
        let annotations = g.annotations();
        assert!(!annotations.is_empty());
        assert!(
            annotations[0].contains("not in the source document"),
            "{annotations:?}"
        );
    }
}

#[cfg(test)]
mod soft_break_regressions {
    use super::*;

    /// Regression (#1756): a bare `=\r` is a TWO-byte soft break. Assuming CRLF
    /// and skipping three bytes stepped into the next character; when that
    /// character was multibyte, the next `text[i..]` sliced mid-scalar and
    /// panicked. A gate that runs over arbitrary captures and arbitrary model
    /// output must not panic on either.
    #[test]
    fn bare_cr_soft_break_before_multibyte_does_not_panic() {
        for s in [
            "=\ré",
            "=\r世",
            "word=\récrit",
            "a=\r\u{1F600}b",
            "=\r",
            "=",
            "=\n",
            "=\r\n",
            "x=\r\ny",
        ] {
            let toks = tokenize(s);
            // Every offset must be a usable slice of the original.
            for t in &toks {
                let _ = &s[t.start..t.end];
            }
        }
    }

    /// Regression (#1756): the over-skip also ate an ordinary ASCII character,
    /// silently manufacturing a token (`ab=\rcd` → `abd`) that exists in
    /// neither the output nor the source. That corrupts the comparison in both
    /// directions — the module unwraps soft breaks precisely so a correct
    /// conversion is not read as fabricated.
    #[test]
    fn soft_break_joins_the_token_without_eating_a_character() {
        assert_eq!(text_of(tokenize("ab=\rcd")), vec!["abcd"]);
        assert_eq!(text_of(tokenize("ab=\ncd")), vec!["abcd"]);
        assert_eq!(text_of(tokenize("ab=\r\ncd")), vec!["abcd"]);
        // The motivating case from the module docs.
        assert_eq!(text_of(tokenize("Zawadow=\nski")), vec!["zawadowski"]);
        assert_eq!(text_of(tokenize("Zawadow=\r\nski")), vec!["zawadowski"]);
        assert_eq!(text_of(tokenize("Zawadow=\rski")), vec!["zawadowski"]);
    }

    /// A lone `=` is not a soft break and must not consume its neighbour.
    #[test]
    fn a_bare_equals_is_a_token_boundary_not_a_break() {
        assert_eq!(text_of(tokenize("a=b")), vec!["a", "b"]);
        assert_eq!(text_of(tokenize("k=1")), vec!["k", "1"]);
        assert_eq!(text_of(tokenize("é=ü")), vec!["é", "ü"]);
    }

    /// The joined token's span still slices the ORIGINAL text, break and all.
    #[test]
    fn joined_token_span_slices_the_original() {
        let s = "Zawadow=\nski wrote";
        let toks = tokenize(s);
        assert_eq!(toks[0].text, "zawadowski");
        assert_eq!(&s[toks[0].start..toks[0].end], "Zawadow=\nski");
    }

    /// End-to-end: a soft-wrapped source and its correct conversion agree.
    #[test]
    fn a_correct_conversion_of_a_soft_wrapped_source_is_grounded() {
        let source = "The report by Zawadow=\r\nski confirms the second quarter fig=\r\nures held.";
        let output = "The report by Zawadowski confirms the second quarter figures held.";
        let g = check(source, output, &GroundingConfig::default());
        assert!(g.is_grounded(), "ungrounded: {:?}", g.annotations());
        assert_eq!(g.grounded_ratio, 1.0);
    }

    fn text_of(toks: Vec<Token>) -> Vec<String> {
        toks.into_iter().map(|t| t.text).collect()
    }
}
