# Speeding up the Windows CI job

**Status:** exploring (iteration 1 landed).
**Scope:** the `windows` job in `.github/workflows/ci.yml` only. No change to
*what* is verified (full-workspace clippy + test on `windows-latest`, the
`#[cfg(windows)]` rot-guard) — only to how long it takes.

## Why

`Windows build + test` is the long pole in CI (~6 min vs ~1.5 min for each
Linux job). Two structural reasons:

1. **One job does two full builds, serially.** On Linux, `Rust lint` and
   `Rust tests` are separate jobs that run in *parallel*. The Windows job runs
   `clippy --all-targets` then `cargo test` back-to-back in a single job, so its
   wall-clock is ~lint+test summed, not max.
2. **Windows is just slower for Rust:** MSVC `link.exe` is slower than LLD/ld;
   Windows Defender real-time scans every `.rlib`/`.pdb`/`.exe` the toolchain
   writes; NTFS small-file churn is expensive; and the cache restore/save over
   NTFS is slower than on Linux.

We can't measure locally — there is no Windows dev box (this job exists
*because* the dev/hook platform can't reproduce it), so the explore loop is:
change → push → read the Windows job time on the PR → iterate.

## Lever inventory (ranked by confidence × impact, lowest risk first)

| # | Lever | Expected effect | Risk | Status |
|---|---|---|---|---|
| 1 | **Defender exclusions** for `target`/`~/.cargo`/`~/.rustup` + `cargo/rustc/link` processes | Large — AV scanning of build output is a top Windows-CI tax | Low (best-effort; no behavior change) | iter 1 |
| 2 | **No debuginfo** (`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`) | Medium-large — skips PDB generation, faster link, less to scan | Low (tests don't need debuginfo) | iter 1 |
| 3 | **`CARGO_INCREMENTAL=0`** | Small — removes incremental bookkeeping on a clean build | Low | iter 1 |
| 4 | **sccache** (GitHub Actions cache backend) | Medium on warm cache; cold first run unaffected | Medium (cache key mgmt; sccache+MSVC quirks) | candidate (iter 2) |
| 5 | **Split clippy / test into parallel jobs** | Up to ~halve wall-clock | Medium (2× runner minutes + 2× cache restore; cache contention) | candidate |
| 6 | **Drop `--all-targets` from the Windows clippy** | Small-medium — fewer test/example/bench crates compiled in the clippy pass (already linted on Linux; `cargo test` recompiles them anyway) | Low-medium (slightly less Windows lint coverage) | candidate |
| 7 | **`rust-lld` as the MSVC linker** | Medium — linking is a big slice on Windows | Higher (stable-MSVC support is fiddly/flaky) | last resort |

## Iteration 1 (this PR)

Levers **1–3**: job-level `env` (debuginfo off, incremental off) + a
Defender-exclusion step before the toolchain/cache steps. Kept clippy+test in
one job so the speedup is attributable to these levers alone, against the ~6 min
baseline.

### Results

| Run | Baseline | After iter 1 |
|---|---|---|
| `Windows build + test` wall-clock | ~5m56s | _TBD — fill from the PR's CI run_ |

## Next, if iteration 1 isn't enough

Reassess with the iter-1 number in hand. If still the long pole, try lever 4
(sccache) and/or lever 5 (split into parallel jobs); treat lever 7 as a last
resort. Record each iteration's number in the table above so we stop when the
marginal win no longer justifies the added complexity.
