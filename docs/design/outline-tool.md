# A portable explicit outline tool

Status: proposal for the operator's 2026-09-26 direction; not implemented.

An outline supplies the structural navigation humans get from an editor:
named sections and definitions, nesting, and source spans. The model chooses
when to use it and which source to inspect next. This is a useful specialized
tool consistent with a small harness: it adds information without owning
the model's workflow.

## Relationship to the existing prototype

[PR #2570](https://github.com/Gilamonster-Foundation/newt-agent/pull/2570)
automatically adds an outline to a large file's first read and to aged reads.
It is held for review. Its pure outline engine, content identity, bounded
rendering, and fixtures are reusable starting points. Its regular-expression
floor estimates a block's end from the following item; those estimates must
not be presented as exact extraction boundaries.

This proposal exposes the operation explicitly. Automatic additions to
`read_file` and compaction remain separate disclosure and evaluation choices.
Do not merge those behavioral changes as a side effect of extracting the
engine. The workspace already declares Rust 1.90, so the prototype's earlier
MSRV blocker is no longer present in this checkout.

## Public surface and ownership

Use one tool named `outline`, with language detection and an optional language
override. Start with Markdown, Rust, Python, and Bash. Avoid separate model
tools for every language; keep each language's extraction rules in its pack.

An illustrative call is:

```json
{"path":"src/worker.rs","depth":2}
```

Return a compact tree of named items with kinds and absolute line spans,
plus source identity and an explicit completeness status. The model can use
ordinary file reads to inspect a selected span. A larger result needs bounded
pagination bound to the same source identity; silently dropping the remainder
would make the map misleading.

Define the rendered-output budget in bytes, including headers, identity,
continuation metadata, and omission notices. Truncate only at UTF-8 boundaries.
The prototype renderer is illustrative, not a proven strict bound: test tiny
budgets and a first item larger than the budget. Structured output retains the
full content identity; an abbreviated display fingerprint cannot bind a cursor.

Create a standalone `agent-outline` crate, initially in this workspace, with
no dependency on `newt-core`, a model provider, session state, or a TUI. Its
core accepts complete raw source bytes and parsing options and returns structured data.
It does not open paths, fetch imports, run code, or acquire authority.
Do not parse rendered read pages, truncation footers, or partially aged text as
if they were the original file. Range-limited output is selected from the
complete parse; the engine contract must not inherit the prototype's rendered
page plus line-offset input or a dependency on Newt's built-in pack registry.

Newt's thin adapter authorizes and captures a file through the existing OCAP
read boundary, then passes those bytes to the engine. An optional
`agent-toolchain` adapter can reuse the same contract. A CLI accepting stdin
and emitting JSON provides a small integration path for other harnesses;
an MCP adapter can follow where a consumer needs it. Each host remains
responsible for authorizing input acquisition and managing resource budgets.

## Structural contract

Each item should identify its kind, name, parent, declaration range, and full
source range. Store byte ranges as well as human-facing 1-based line spans;
this avoids ambiguity around Unicode, CRLF, and the final newline. Define
the byte ranges as half-open. Include attributes/decorators in a full range
only according to a tested language-specific policy.
Display line ends are inclusive of lines containing bytes in the span: an end
at column zero of the following line belongs to the preceding line. Empty
ranges need an explicit point representation. A Markdown section extends to
the next heading of equal or higher level, or EOF, rather than ending with its
heading node. Test skipped heading levels and a file without a final newline.

| File type | Initial outline content | Boundary cases to prove |
| --- | --- | --- |
| Markdown | Heading hierarchy and section ranges | Setext headings, fenced code, headings inside code fences |
| Rust | Modules, types, traits, implementations, methods, functions | Attributes, multiline signatures, nested modules, macros, braces in strings/comments |
| Python | Classes, functions, methods, nested definitions | Decorators, async definitions, multiline signatures, indentation |
| Bash | Function definitions and their bodies | Both function syntaxes, nested compounds, quoting, here-documents |

Use syntax parsers for code and a Markdown parser for document structure.
[Tree-sitter's query and tagging facilities](https://tree-sitter.github.io/tree-sitter/4-code-navigation.html)
are a suitable foundation for language packs. Capture full definition nodes
as well as identifier nodes; an identifier's range is not its body's range.
Initially prefer a small set of bundled grammars over a new runtime grammar
distribution system. Runtime-loaded grammars can be evaluated separately.

An outline describes source syntax. It does not claim compiler name resolution,
macro expansion, dynamically created definitions, or a complete call graph.
Malformed input may return a partial outline with parse diagnostics. Unsupported
language, empty outline, truncation, and parse failure are distinct outcomes.

Reuse the existing content-addressing implementation for the exact input
bytes; do not invent a second hash or CID format. Cache identity must also
include the parser/grammar/query version and options. A changed source invalidates
its old spans. A content identity proves which bytes were outlined and does
not grant permission to retrieve them.

## Delivery and evidence

1. Extract the pure engine contract and fixtures into the independent crate.
   Preserve the PR's authorship and keep automatic read/compaction behavior
   out of the extraction. Implement parser-backed spans for the initial four
   languages before describing them as exact.
2. Add the Newt `outline` adapter through the same authorized-read path as
   `read_file`, with consistent byte and output bounds, cancellation, and audit.
   Add the stdin/JSON CLI without any Newt runtime dependency.
3. Exercise real fixtures on Linux, macOS, and native Windows. Verify span
   slicing, nesting, parse errors, Unicode/CRLF, unsupported files, changed
   source, and denied reads with no extra file access or code execution.
4. Rerun the ordinary largest-file refactor prompt with the same model, seed,
   budgets, and grants. Advertise the tool normally; do not tell the model it
   must use it. Record actual adoption, repeated reads, tokens, elapsed time,
   edits, and verification. Compare task completion as well as tool-call count.
5. Evaluate automatic outline-on-read or outline-on-age independently if still
   desired. The explicit tool should earn its place through useful task evidence.
