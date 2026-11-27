# Decision: newt-core's confinement dependencies are optional

**Status:** Accepted — #3160, R1 (explicit runtime bypass).
**Date:** 2026-11-24 (UTC); shared-leaf amendment accepted 2026-11-27.
**Related:** [Confinement model](ocap_confinement_model.md),
[deviation register](../security/ocap-deviations.md#confine-off-build).

## Feature contract

`newt-core` enables `markdown` and `confine` by default. `confine` activates
three optional normal dependencies: `agent-bridle`, `agent-bridle-tool-shell`
and `agent-bridle-tool-web`. The owner-approved rule is: **no
agent-bridle-core, agent-bridle-tool-shell or the agent-bridle facade without
`confine`**. Small leaf crates may be shared: the selected normal/build closure
retains `agent-bridle-fdguard` for descriptor hygiene and held directory handles.
This replaces the original prohibition on every `agent-bridle-*` package.
Both platform features,
`windows-appcontainer` and `macos-seatbelt`, still enable `confine` and
forward the corresponding bridle feature.

External consumers using `default-features = false` must explicitly request
`features = ["confine"]` to retain the compiled backend. Cargo features are
additive: another consumer requesting defaults can enable it again.
Repository consumers still request core's defaults. This decision proves
**newt-core's selected closure**, not a bridle-free `newt` executable, release
binary or Python wheel. Downstream feature threading is separate work (P15a).
Use “confine off” for this configuration; “lean” also names markdown and TUI
configurations with different feature selections.

## Behavior without confine

Compilation without a backend does not assert `ocap_disabled()` (R1).
Host shell admission still requires the explicit launch-time OCAP bypass,
no broker, no execution lease, and a permitting execution floor. Full access
alone, or selecting the Host engine, is insufficient. Earlier validation,
routed built-ins and native operations retain their own boundaries.

| Capability | Confine-off behavior and evidence |
|---|---|
| Executor calls, including trusted infrastructure fetches | `ConstrainedExecutor::run` and `run_async` return `ConfinementUnenforceable`; the `executor_off` tests pin the typed refusal and absent probes. No executor host fallback. |
| Commands reaching the shell boundary | `ExecOutcome::Unavailable`, before backend execution, grants or scratch preparation; `confine_off_tests` covers ordinary/build commands, the host floor and recorder. |
| Governed Git | Brokers and all their preflight refusals, data-loss confirmations and network-read prompts are absent. OCAP-on commits retain the no-broker refusal; other commands reaching the shell boundary refuse. Plain host publication and its existing bypass notice require the explicit runtime bypass and route conditions above. The off commit and quoted-commit tests pin refusal. |
| Build lane and `move_from` | The build helper refuses before its execution line and permission request; earlier public tool presentation may still occur. `move_from` refuses before its transaction. The off lifecycle/move tests pin these helper/dispatch boundaries. |
| Posture and dispatch | The executor verifier reports `Absent` with the missing-feature reason; `ocap_l3_backend` reports `(none, false)`. `maybe_dispatch` returns `None` (macOS alias initialization remains). The verifier, L3 and truth tests pin the posture. |
| Core `web_fetch`, file tools and policy | Retained: the owned fetch implementation, newt-native file fence, shell inspection, durable signed policy, step-up policy and `agent-bridle-fdguard`. Runtime implicit sandbox roots are empty when no backend is compiled; the off root test pins that fact. |

The catalog can still advertise operations that this build refuses; catalog,
alias and steering-text changes remain separate work. Default backend runtime
residuals, including its documented advisory fallback, are unchanged. An
optional build posture does not close a kernel-confinement deviation.

The MCP crates each have their **own** default-on `confine` feature. Disabling
core's feature alone does not remove MCP server tools or change client spawn
posture. With the client's own feature off, stdio is advisory with environment
scrubbing and Unix descriptor hygiene, and protected workspaces refuse it;
HTTP uses its pinned client without the loopback proxy. With the server's own
feature off, `shell_run` and `web_fetch` are absent. Its default fetch remains
gated to preserve the registry's session call budget. Their dependencies still
request core defaults until downstream threading changes that.

## Proof and feature matrix

| Selection | Gate |
|---|---|
| Default core | Linux all-target Clippy and default library suite; native platform CI |
| `--no-default-features --features confine` | Markdown-off all-target Clippy |
| `--no-default-features --features markdown` | Core alone: native all-target Clippy on Linux, Windows and macOS; guarded refusal/route tests and feature probe on Linux |
| `--no-default-features` | Independent normal/build closure absence assertion for each target configuration below |

The existing `closure_ledger` integration target invokes Cargo for both absence
selections and a measured default positive control on each target below. The
default controls name five bridle crates on every target, including the shared
fdguard (whose descriptor sweep is a no-op on Windows). Off checks forbid only
the three confinement backend packages named above. The ledger also inspects
metadata covering every direct normal/dev/build/target declaration and renamed
local key for those packages. Its literal sample test is hermetic; the Cargo-backed tests are integration
evidence. Counts are cached by package **and feature selection**. The ordinary
ledger target runs in CI and the hook; its off closure checks do not depend on
the test executable itself having confine off.

The closure is **resolved for three target configurations on a Linux runner**
with `cargo tree --target <triple> -e normal,build`; this does not compile for
those targets or require their Rust standard libraries. Both bare off and
markdown-only off are checked. The package-count ceilings remain Linux-only.
Native lint and test steps select `--no-default-features --features markdown`:

| Target configuration | Off closure resolution | Native off lint in CI | Native off tests in CI |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | Linux ledger | Linux `lean-build` job | Guarded refusal/route selection |
| `x86_64-pc-windows-msvc` | Linux ledger | `windows` job | None |
| `aarch64-apple-darwin` | Linux ledger | `macos` job | None |

The native lint steps establish compilation independently of closure
resolution. Only Linux runs native off tests in CI.

The off library guard checks listing success, subtracts ignored matches and
requires at least 18 `confine_off` tests on the Linux/Unix selection measured
at the P12 predecessor. Other OR-filter terms cannot satisfy that floor.
The same guarded selection runs in CI and Unix `just check`. Real executor
integration targets require `confine`; the truth target retains its two
non-executor tests off. Default parity tests still compare the owned code
with bridle; those comparisons are omitted off.

The temporary lexical boundary scanner is retired. Off compilation rejects
unresolved bridle references in compiled targets; closure/metadata tests
reject dependency leaks. Those checks do not establish that no upstream
source has been copied inline: the repository's reuse/copy rules still apply.

Re-measure the feature ledger with this command (after checking Cargo succeeds):

```sh
cargo tree --color never -p newt-core --no-default-features --features markdown \
  -e normal,build --prefix none --target x86_64-unknown-linux-gnu
```

Strip trailing ` (*)`, deduplicate, and count. Measurements on 2026-11-27:

| Core selection | Before sharing fdguard | After |
|---|---:|---:|
| Defaults | 526 | 525 |
| `--no-default-features` | 416 | 416 |
| `--no-default-features --features markdown` | 417 | 417 |

The default and markdown-only ceilings remain 530 and 430; a ceiling may only go down.

The vendored Linux `build_isolation` module stays available without a new Cargo
feature. Its `seccompiler` and `linux-raw-sys` package versions already occur in
the off closure through `newt-policy` and `rustix`; sharing it adds no dependency
package beyond replacing the descriptor guard itself. A new feature would not
reduce this closure. Runtime build isolation still requires the confinement
backend; importing the leaf installs no hooks by itself.

## Shared features and packaging limits

Core explicitly requests reqwest `gzip`, `charset` and `rustls-tls`, alongside
its inherited `rustls-tls-native-roots`, `json` and `stream`. This preserves
the previously unified decoding and bundled/native trust-root feature set;
the trust-root consequence is inferred from reqwest's feature table, not a TLS
experiment. The default/off feature-tree difference is recorded below.

In the normalized `cargo tree -e features --prefix none` difference between
the two selections, the removed packages and their activation edges belong to
bridle's shell, carried coreutils, OS sandbox and P-256 verifier stacks. Several names have
multiple versions: `nix` 0.31.3, `signature` 3.0.0, `serde_spanned` 0.6.9 and
`toml_edit` 0.22.27 disappear while their other versions remain. Their lost
activation edges do not mean the surviving versions lost those features.

A second comparison, normal/build trees with `--format '{p}|{f}'` on the
pinned Linux target, reads resolved features per package version. These are
all losses on surviving packages (empty feature lists are not features):

| Packages | Features lost | Disposition |
|---|---|---|
| `newt-core` | `confine`, `default` | Requested selection; markdown stays. |
| `clap` 4.6.1, `clap_builder` 4.6.0 | `cargo`, `env`, `wrap_help` | Extra shell CLI APIs and help wrapping leave with their consumers; no new core feature request. |
| `crypto-common` 0.2.2; `digest` 0.11.3; `hybrid-array` 0.4.14; `subtle` 2.6.1 | respectively `rand_core`; `mac`; `subtle`, `zeroize`; `const-generics` | Extra generation/MAC/constant-time/zeroize trait APIs used by the removed verifier. Source inspection shows hybrid-array's zeroize feature supplies trait impls, not an automatic Drop action. Retained callers must still compile; no fallback algorithm is introduced. |
| `rustix` 1.1.4 | `net`, `param`, `pipe`, `process`, `system`, `termios`, `thread` | Shell/sandbox syscall APIs; core retains its filesystem APIs. |
| `linux-raw-sys` 0.12.1 | `if_ether`, `net`, `netlink`, `prctl`, `system`, `xdp` | Bindings behind the removed syscall APIs. |
| `icu_locale_core`, `icu_provider`, `litemap`, `yoke`, `zerofrom`, `zerotrie` | `alloc` | Additional owned-data APIs from carried coreutils' internationalization stack. |
| `potential_utf` 0.1.5; `writeable` 0.6.3 | `alloc`, `writeable`; `alloc`, `either` | The same formatting/owned-data stack's adapters. |
| `nom` 7.1.3; `minimal-lexical` 0.2.1 | `default`, `std`; `std` | Extra standard-library parser APIs; retained parsers still compile with their selected allocation support. |
| `nu-ansi-term` 0.50.3 | `gnu_legacy` | Accepted off-only ANSI spelling difference: no GNU leading zero and background/foreground ordering follows the crate's nonlegacy branch. No default-output change is claimed. |
| `smallvec` 1.15.2 | `union` | Internal storage representation; its public collection operations remain. |
| `rand` 0.8.7 | `small_rng` | Optional SmallRng API used by removed shell dependencies. |
| `rand` 0.10.2; `num-traits` 0.2.19; `either` 1.16.0 | `default`; `i128`; `use_std` | Marker/compatibility features; surviving resolved component features stay, and either retains `std`. |

No resolved reqwest, libc, tokio, serde, serde_json, sha2,
content-addressable or agent-mesh-protocol feature is lost. In particular,
libc `extra_traits` remains. These dependency/source observations are not a
TLS, ANSI-rendering or cryptographic runtime experiment. The feature matrix
and library runs check the selected callers.

The workspace's three bridle patches remain necessary for Newt-specific APIs.
Making dependencies optional does not repair published-crate compilation
(#3181/#3163), publish those patches, change `macos_private_ptys` or ApplyToken,
or publish the removed descriptor-guard copy. That copy has no crates.io release;
the shared leaf remains resolved through the existing vendor patch. Its former
differential tests now use literal behavior expectations, real filesystem
observations and an unguarded child control. The identical hostile-descriptor
suite runs once in the shared crate; there is no byte-identity twin to maintain.
Consumer closure proofs use normal/build edges, while metadata rejects hidden
direct confinement-backend dev/build dependencies in core itself.
