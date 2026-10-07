# Native carried tools

Standalone ambient command pack for #2776. This crate does not change Newt's
shell routing or authorize any confined/private worker dispatch. License follows
the workspace; uutils 0.12.0 is MIT (LICENSE-uutils). The ripgrep archive carries
its own COPYING, LICENSE-MIT and UNLICENSE, retained by the installer.
BusyBox-w32 is GPLv2 (LICENSE-busybox); its corresponding-source URL ships alongside.

## Build and install (Windows x86-64)

```powershell
cargo build -p newt-carried-tools --release --locked
cargo build -p newt-cli --bin newt-ambient-brush --release --locked
./scripts/windows/Install-CarriedTools.ps1 `
  -BinaryPath target/release/newt-carried-tools.exe -NewtDirectory <release-dir>
Copy-Item target/release/newt-ambient-brush.exe <release-dir>/tools/
./scripts/windows/Test-CarriedTools.ps1 -ToolsDirectory <release-dir>/tools
```

The release directory must exist and must not contain `tools`. Installation
stages into a temporary directory, verifies the pinned upstream archive before
extraction, then moves the completed tools directory into place. It never edits
PATH or overwrites a live installation. `-RipgrepArchive` permits offline use of
the exact same verified archive. `-BusyboxArchive` similarly accepts the pinned
BusyBox executable (upstream distributes it uncompressed). Build the multicall for the destination OS.

PR 2's layout contract, relative to `newt.exe`:

```text
tools/newt-carried-tools.exe
tools/{cat,head,tail,wc,ls,mkdir,rm,cp,mv,sort,uniq,tr,cut,echo,grep,which}.exe
tools/{sed,awk,xargs}.exe
tools/newt-ambient-brush.exe
tools/rg.exe
tools/ripgrep-{COPYING,LICENSE-MIT,UNLICENSE}
tools/LICENSE-uutils
tools/LICENSE-busybox
tools/busybox-SOURCE.txt
```

Per-name executables are hardlinks where supported, otherwise byte copies.
Archives that do not preserve hardlinks may expand their size; installers can
recreate them using this script. Invocation dispatches by executable basename.
`grep.exe` invokes only the absolute adjacent `rg.exe`, with inherited stdio,
never a shell or PATH search. PR 2 must prepend this tools directory to its
sanitized child PATH so descendants can also resolve names. Only sed, awk and xargs are exposed from BusyBox: there is no busybox.exe,
sh.exe or applet installation step. This limits PATH exposure, not the compiled
capabilities of the upstream multicall binary.

All 14 uutils crates are pinned to 0.12.0. The installer obtains upstream ripgrep
14.1.1 x86_64-pc-windows-msvc.zip, pinned SHA256
`d0f534024c42afd6cb4d38907c25cd2b249b79bbe6cc1dbee8e3e37c2b6e25a1`.
This is a foreign upstream checksum, verified using the same SHA256 pattern as
Newt's Chocolatey packaging, not a new Newt artifact identity scheme. Update the
version, archive pin and native evidence together. Other architectures are not
packaged by this installer. This PR provides staging; wiring release distribution
and default-on shell selection belongs to the following integration.

## Added commands (#2795)

BusyBox-w32 **FRP-6075-g169694ebd**, x64 Unicode build (Windows 10 1903+),
is pinned at `https://frippery.org/files/busybox/busybox-w64u-FRP-6075-g169694ebd.exe`
with SHA256 `6e263d154d8548d1eb936f65d1d8312c80df31c45974e48d6335e4dcc0f4f34c`.
The GPLv2 license is copied verbatim from that release's source tarball.
`busybox-SOURCE.txt` points to its corresponding source tarball on frippery.org.
Checksums are verified before copying executables into the completed pack;
corruption aborts installation and removes staging. Native tests cover renamed
hardlinks and independent copies, plus Brush pipelines and quoting for
`sed -n '2,3p'`, `sed -i 's/x/y/'`, `awk '{print $1}'`, `xargs -n1`, and `xargs -I{}`.
These are BusyBox applets, not a claim of full GNU utility compatibility.

`which [-a] [--] command ...` is implemented here with no additional dependency.
It searches PATH in order and, on Windows, PATHEXT in order (default
`.COM;.EXE;.BAT;.CMD` when unset). Explicit paths are checked directly;
recognized extensions are not appended twice. `-a` prints every distinct match;
by default only the first is printed. Exit 1 means at least one command was not
found; malformed options or no names exit 2. On Unix, matches must be regular
files with executable permission bits. On Windows, files must have a PATHEXT
extension. It reports paths, never executes the located commands.

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
  `^` anchors only at pattern start and `$` only at pattern end; elsewhere
  they are literal. A leading `*`, including immediately after the initial
  `^`, is literal. Other `*` operators repeat the preceding atom; ambiguous
  repeated quantifiers (for example `a**`) are refused. Groups, including
  `\(*\)`, remain outside this subset and receive a supported-set refusal.
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
