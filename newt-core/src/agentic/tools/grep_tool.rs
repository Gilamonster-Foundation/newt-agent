//! In-process regex line search for the embedded `grep` tool.
//!
//! Runs on ripgrep's own library crates: `grep-regex` matches and
//! `grep-searcher` searches (line numbers, context, binary detection, any
//! file size, invalid UTF-8 shown lossily). No shell and no subprocess, so it
//! needs only `fs_read`, like `read_file`, and keeps working where every
//! `run_command` spawn is refused (notably macOS). The walk is `find`'s
//! (`workspace_walker`); the execution arm in `tools.rs` reuses `find`'s
//! permission checks.

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};

/// Options parsed from the tool's JSON args.
pub(crate) struct GrepOpts<'a> {
    pub pattern: &'a str,
    pub glob: Option<&'a str>,
    pub ignore_case: bool,
    pub context: usize,
    pub max_results: usize,
}

/// The default result cap: the same floor `find` advertises.
pub(crate) const DEFAULT_MAX_RESULTS: usize = 100;

/// Output lines in grep's own shape (`path:N:text` for a hit, `path-N-text`
/// for context, `--` between separated groups) and whether `max_results`
/// stopped the search early.
pub(crate) struct GrepOutput {
    pub lines: Vec<String>,
    pub truncated: bool,
}

/// Search every file under `root` that the workspace walk yields. `Err` only
/// for an invalid pattern or glob; an unreadable file is skipped, like `find`.
pub(crate) fn grep_search(
    root: &std::path::Path,
    workspace_root: &std::path::Path,
    opts: &GrepOpts<'_>,
) -> Result<GrepOutput, String> {
    if opts.pattern.is_empty() {
        return Err("`pattern` is required".to_string());
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.ignore_case)
        .build(opts.pattern)
        .map_err(|e| format!("invalid pattern: {e}"))?;
    let glob = match opts.glob.filter(|g| !g.is_empty()) {
        Some(g) => Some(
            globset::Glob::new(g)
                .map_err(|e| format!("invalid glob: {e}"))?
                .compile_matcher(),
        ),
        None => None,
    };
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .before_context(opts.context)
        .after_context(opts.context)
        .build();
    let mut sink = Collect {
        rel: String::new(),
        out: GrepOutput {
            lines: Vec::new(),
            truncated: false,
        },
        hits: 0,
        max: opts.max_results,
    };
    for entry in super::workspace_walker(root, true, None).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(workspace_root)
            .unwrap_or_else(|_| entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if glob.as_ref().is_some_and(|g| !g.is_match(&rel)) {
            continue;
        }
        sink.rel = rel;
        search_one(&mut searcher, &matcher, entry.path(), &mut sink);
        if sink.out.truncated {
            break;
        }
    }
    Ok(sink.out)
}

fn search_one(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &std::path::Path,
    sink: &mut Collect,
) {
    let before = sink.out.lines.len();
    // An unreadable file is skipped, as `find` skips one; a binary file
    // (NUL byte) stops early. Neither leaves partial output behind.
    let ok = searcher.search_path(matcher, path, &mut *sink).is_ok();
    if !ok {
        sink.out.lines.truncate(before);
    }
}

struct Collect {
    rel: String,
    out: GrepOutput,
    hits: usize,
    max: usize,
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\n', '\r'])
        .to_string()
}

impl Sink for Collect {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.hits >= self.max {
            self.out.truncated = true;
            return Ok(false);
        }
        self.hits += 1;
        let n = m.line_number().unwrap_or(0);
        self.out
            .lines
            .push(format!("{}:{n}:{}", self.rel, text(m.bytes())));
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if matches!(c.kind(), SinkContextKind::Other) {
            return Ok(true);
        }
        let n = c.line_number().unwrap_or(0);
        self.out
            .lines
            .push(format!("{}-{n}-{}", self.rel, text(c.bytes())));
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, Self::Error> {
        self.out.lines.push("--".to_string());
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, Self::Error> {
        // Binary file: report nothing from it, like `grep -I`.
        Err(std::io::Error::other("binary"))
    }
}
