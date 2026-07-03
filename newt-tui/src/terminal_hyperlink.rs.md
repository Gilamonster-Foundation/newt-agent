# `terminal_hyperlink` — OSC 8 Clickable URLs in Terminals

Implements [OSC 8](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cbecd68) terminal hyperlinks for Newt's TUI output.

## How it works

Modern terminals (iTerm2, WezTerm, GNOME Terminal, Alacritty, Kitty, VS Code) support OSC 8 escape sequences that render as clickable URLs:

```
\x1b]8;;https://example.com\x1b\\clickable text\x1b]8;;\x1b\\
```

This module provides `render_link(text, url)` which wraps terminal output in OSC 8 sequences when the user is running Newt against an actual terminal (not a pipe or CI).

## Usage

```rust
use newt_tui::terminal_hyperlink::{supports_osc8, render_link};

if supports_osc8() {
    println!("{}", render_link("View issue", "https://github.com/org/repo/issues/771"));
} else {
    println!("View issue: https://github.com/org/repo/issues/771");
}
```

## Testing

Unit tests are included in this module. Integration test at `tests/terminal_hyperlink_integration.rs`.

Manual OSC 8 verification (run in a terminal):

```bash
./test_osc8.sh   # from .newt/sessions/.../ directory
```

## Compatibility

| Tier | Terminals |
|------|-----------|
| **Safe** | VS Code, iTerm2, WezTerm, GNOME Terminal 3.46+, Alacritty 0.13+, Kitty |
| **Opt-in** | Foot, Tilix, Windows Terminal (recent), Konsole 22.12+ |
| **Unsafe** | tmux < 3.3 without OSC 8 patch, screen, old terminals |

## References

- [OSC 8 spec](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cbecd68)
- [GitHub issue #771](https://github.com/Gilamonster-Foundation/newt-agent/issues/771)
