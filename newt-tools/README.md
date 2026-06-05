# newt-tools

Newt-Agent file/code tool surface — read, edit, search, apply-patch.

The vi-minimal v0 tool set: `read`, `edit`, `search`, `apply_patch`, plus
directory listing. These are deliberately thin wrappers; when `thoon-fileops`
publishes, this crate delegates to it rather than reimplementing.

Key capabilities:

- `read` / `list_dir` — file and directory access
- `search` — regex search across a tree
- `apply_patch` / `apply_whole_files` / `edit` — patch application with
  pluggable appliers (fuzzy by default, `diffy` behind a feature)

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
small, fast, local-first agentic coder.

## License

Apache-2.0
