//! Visual smoke test for the Markdown → ANSI renderer (Steps 24.1–24.2, #559).
//!
//! Renders a sample document the way newt will render assistant output in the
//! chat scroller — styled, word-wrapped, plain-scrolled (no widgets).
//!
//!   cargo run -p newt-core --example render_md            # built-in sample
//!   cargo run -p newt-core --example render_md -- FILE     # render your own
//!   cargo run -p newt-core --example render_md -- --raw    # show the source
//!   cargo run -p newt-core --example render_md -- --stream # watch it stream
//!
//! Eyeball it: headings orange+bold, emphasis, dim code/quote/borders, lists
//! and a box-drawing table that fits the current terminal width. `--stream`
//! drives the same content through `MarkdownStreamWriter` in small chunks so
//! you can see block-aware streaming (inline lines appear live; fences/tables
//! hold until they close) the way the live chat loop will.

use newt_core::agentic::{render_markdown, MarkdownStreamWriter, RenderOpts};
use std::io::Write;

const SAMPLE: &str = r#"# Newt Markdown renderer

A quick tour of what the **plain-scroller** renderer does — *inline* emphasis,
~~strikethrough~~, `inline code`, and a [link](https://example.com) all reflow
to the terminal width without any widget surface.

## Lists

- top-level bullet
- another, with **bold** inside
  - nested item one
  - nested item two
- [x] a finished task
- [ ] a pending task

1. first ordered
2. second ordered

## Blockquote

> The instrument is not the thing being observed.
> Dim bars, wrapped to width.

## Code

```rust
fn main() {
    println!("dim, inset, never reflowed");
}
```

## Table

| Crate   | Command | Status |
| :------ | :-----: | -----: |
| cargo   | build   | ok     |
| just    | check   | green  |
| 日本語テスト | cjk   | wide   |

---

That `---` above is a thematic break.
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let raw = args.iter().any(|a| a == "--raw");
    let path = args.iter().find(|a| !a.starts_with('-'));

    let src = match path {
        Some(p) => std::fs::read_to_string(p).unwrap_or_else(|e| {
            eprintln!("render_md: cannot read {p}: {e}");
            std::process::exit(1);
        }),
        None => SAMPLE.to_string(),
    };

    if raw {
        print!("{src}");
        return;
    }

    let cols = crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .max(20);

    if args.iter().any(|a| a == "--stream") {
        stream_demo(&src, cols);
        return;
    }

    // `▸  ` mirrors the leader newt prints before assistant output.
    println!("\n▸  (rendered at {cols} cols)\n");
    println!(
        "{}",
        render_markdown(&src, RenderOpts { color: true, cols })
    );
    println!();
}

/// Feed the source through the streaming writer in small chunks, with a short
/// delay, so block-aware streaming is visible.
fn stream_demo(src: &str, cols: usize) {
    let mut stdout = std::io::stdout();
    print!("\n▸  ");
    stdout.flush().ok();
    let mut w = MarkdownStreamWriter::new(&mut stdout, RenderOpts { color: true, cols });
    // Chunk on whitespace boundaries to mimic token deltas.
    let mut chunk = String::new();
    for ch in src.chars() {
        chunk.push(ch);
        if ch == ' ' || ch == '\n' {
            w.push(&chunk).ok();
            chunk.clear();
            std::thread::sleep(std::time::Duration::from_millis(18));
        }
    }
    w.push(&chunk).ok();
    w.finish().ok();
    println!();
}
