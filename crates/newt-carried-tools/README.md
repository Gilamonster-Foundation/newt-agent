# Native carried tools

Standalone ambient command pack for #2776. This crate does not change Newt's
shell routing or authorize any confined/private worker dispatch. License follows
the workspace; uutils 0.12.0 is MIT (LICENSE-uutils). The ripgrep archive carries
its own COPYING, LICENSE-MIT and UNLICENSE, retained by the installer.

## Build and install (Windows x86-64)

```powershell
cargo build -p newt-carried-tools --release --locked
./scripts/windows/Install-CarriedTools.ps1 `
  -BinaryPath target/release/newt-carried-tools.exe -NewtDirectory <release-dir>
./scripts/windows/Test-CarriedTools.ps1 -ToolsDirectory <release-dir>/tools
```

The release directory must exist and must not contain `tools`. Installation
stages into a temporary directory, verifies the pinned upstream archive before
extraction, then moves the completed tools directory into place. It never edits
PATH or overwrites a live installation. `-RipgrepArchive` permits offline use of
the exact same verified archive. Build the multicall for the destination OS.

PR 2's layout contract, relative to `newt.exe`:

```text
tools/newt-carried-tools.exe
tools/{cat,head,tail,wc,ls,mkdir,rm,cp,mv,sort,uniq,tr,cut,echo,grep}.exe
tools/rg.exe
tools/ripgrep-{COPYING,LICENSE-MIT,UNLICENSE}
tools/LICENSE-uutils
```

Per-name executables are hardlinks where supported, otherwise byte copies.
Archives that do not preserve hardlinks may expand their size; installers can
recreate them using this script. Invocation dispatches by executable basename.
`grep.exe` invokes only the absolute adjacent `rg.exe`, with inherited stdio,
never a shell or PATH search. PR 2 must prepend this tools directory to its
sanitized child PATH so descendants can also resolve names. No xargs is shipped:
it belongs to findutils and needs its own native validation.

All 14 uutils crates are pinned to 0.12.0. The installer obtains upstream ripgrep
14.1.1 x86_64-pc-windows-msvc.zip, pinned SHA256
`d0f534024c42afd6cb4d38907c25cd2b249b79bbe6cc1dbee8e3e37c2b6e25a1`.
This is a foreign upstream checksum, verified using the same SHA256 pattern as
Newt's Chocolatey packaging, not a new Newt artifact identity scheme. Update the
version, archive pin and native evidence together. Other architectures are not
packaged by this installer. This PR provides staging; wiring release distribution
and default-on shell selection belongs to the following integration.

## Bounded grep contract

Supported short flags (including clusters): `-r -R -E -F -n -c -v -i -l -w -o
-H -h -e PATTERN`. `-ePATTERN`, repeated `-e`, newline-separated patterns and
`--` work. Paths retain native OS encoding; patterns must be UTF-8. Unsupported
options and missing operands exit 2 with the supported set and a direct-rg hint.
No GNU long-option surface is claimed.

- `-r` recurses; `-R` additionally follows symlinks. Without either, directory
  operands are refused. No file operands means stdin, or `.` with recursion.
- Hidden and ignored files are searched. Input is treated as text, including
  binary input; GNU binary-file messages and locale collation are not provided.
- Default regex supports literals, `.`, `*`, anchors and simple bracket classes.
  ERE-only punctuation is escaped, so `alpha|beta` is literal in default mode.
  BRE escapes for groups, repetitions, word boundaries or backreferences and
  nested/POSIX bracket syntax are rejected. Use `-E` for rg's regex dialect
  (no backreferences/lookaround) or `-F` for fixed strings. `-E` and `-F` conflict.
- `-c` counts matching lines, including printing `0` for a no-match file;
  no-match still exits 1. `-l` overrides count/only-matching in either order;
  count overrides only-matching. `-vo` suppresses output but completes the
  search to retain errors. `-n`, `-v`, `-i`, `-l`, `-w`, `-o`, `-H`, `-h`
  retain their usual meanings within this text/regex contract.
- rg configuration files, headings, automatic color and automatic line numbers
  are disabled. `-n` explicitly restores line numbers. Exit 0 is a match, 1 no
  match, 2 an argument/search/spawn error. stdout/stderr stream directly.

Portable tests protect argv semantics; the Windows CI package lane grounds them
with real installed executables, including the former silent `-rn` corruption,
`-E` encoding confusion, no-match counts, and command names with a spaced cwd.
