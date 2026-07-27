# Language packs (workspace API surface)

The `knowledge_base` technique injects an **authoritative API surface** of your
workspace into the model's frozen system prompt — the real public symbols, so the
model grounds against them instead of inventing APIs. *How* a language's public
symbols are recognized is a **language pack**: pure data, so adding a language is
config, not a code change. The same registry is the harness definition of
**source/code files** for source-first repository investigation, including
`find(category="source", language="C++")`; docs, manifests, lockfiles, and
generated artifacts remain supporting evidence rather than substitutes for
code unless the operator explicitly requests them.

> **Extraction engines.** Today a pack extracts with **regex** rules (zero deps,
> the bootstrap). The right long-term engine is an **AST parser (tree-sitter)** —
> a pack will be able to declare a tree-sitter query instead of regex rules, behind
> an opt-in `ast` feature. The pack model below is engine-agnostic (`extensions` /
> `entry_points` / merge-by-name / drop-in loading don't change); only the
> per-pack extraction rules change shape (regex → query). See the AST follow-up.

## Built-in packs (first-class)

Rust, Python, Bash, C/C++, C#, Go, Java, Ruby, Dart, and TypeScript ship built
in (`newt-core::api_surface::builtin_packs`). They double as worked examples —
copy one and adapt.

## Adding / overriding a pack

Packs merge **by `name`** across these layers (later wins):

1. **Built-ins** (in the binary).
2. **Global drop-in**: `~/.newt/language-packs/<name>.toml`.
3. **Project drop-in**: `.newt/language-packs/<name>.toml`.
4. **Inline config**: `[[context.api_surface.language_packs]]` in `config.toml`.

A pack named `rust` *replaces* the built-in `rust`; a new name *adds* a language.
So you can ship a Ruby pack, sharpen the C/C++ rules, or override anything —
**without touching the binary**.

## Schema

```toml
name = "ruby"                 # stable id (a built-in's name replaces that built-in)
aliases = ["ruby", "rb"]      # human spellings for prompt/tool language filters
extensions = ["rb"]           # file extensions, no dot

# Entry-point file globs — these files are the public API, listed first in the
# surface. Globs: exact ("lib.rs"), suffix ("*.h"), or all ("*"). Optional.
entry_points = ["*.rb"]

# Public-symbol extraction rules (regex bootstrap). Each rule's FIRST capture
# group is the symbol name; `kind` is a free-form label (fn / class / struct /
# func / method / …) shown in the surface.
[[symbols]]
pattern = '^\s*def\s+([a-z_]\w*)'
kind = "method"

[[symbols]]
pattern = '^\s*class\s+(\w+)'
kind = "class"
```

Drop that at `~/.newt/language-packs/ruby.toml` and `.rb` files start contributing
to the surface — no rebuild. A malformed pack file is skipped with a warning
(never fatal), and an invalid regex in a rule is dropped (the rest of the pack
still works).

## Budget

The surface rides every turn's system prompt, so it's bounded:

```toml
[context.api_surface]
max_block_chars = 3000        # hard ceiling on the rendered block
max_symbols_per_file = 12     # per-file cap, so one huge file can't crowd it out
```

## Examples

- `examples/language-packs/TEMPLATE.toml` — a commented starting point.
- `examples/language-packs/ruby.toml` — a complete worked example.
