//! `newt timer` — the self-scheduled wake-up surface.
//!
//! `schedule` records a deferred prompt; `list` shows the queue; `fire` drains
//! due timers; `dismiss` cancels a watch. The agent is not resident between
//! turns, so the **host** owns the clock and re-enters the model here. Reuses
//! the pure decision core in [`newt_core::timer`].
//!
//! ## Wake-up path (the supported one)
//!
//! `newt timer fire --run` drains due timers and drives each prompt through
//! the same [`crate::solve::run`] entry point `newt solve` uses — writing the
//! prompt to a transient `--instruction-file` and running a real headless
//! solve against the current workspace. A host scheduler (cron, launchd, a
//! shell loop) only has to call this on a beat:
//!
//! ```sh
//! */5 * * * * newt timer fire --run --dir ~/.newt
//! ```
//!
//! Without `--run`, `fire` is a **non-mutating emit**: it prints one record
//! per due timer as `PROM\t<id>\t<prompt>` for a host that wants to feed a
//! different entry point. It does NOT consume the queue — a due timer is not
//! removed merely for being emitted, so the host MUST `newt timer dismiss <id>`
//! once its own solve succeeds, or every beat re-emits the same prompt. The id
//! is in the record precisely so that dismiss is possible; the prompt field is
//! escaped (`\\` `\n` `\r` `\t`) so one timer is always exactly one line:
//!
//! ```sh
//! newt timer fire | while IFS=$'\t' read -r _ id prompt; do
//!   f=$(mktemp); printf '%b' "$prompt" > "$f"
//!   newt solve --instruction-file "$f" && newt timer dismiss "$id"; rm -f "$f"
//! done
//! ```
//!
//! Prefer `--run` unless you need a different entry point: it owns the whole
//! claim → execute → acknowledge lifecycle, including the claim token that
//! stops a stalled beat from consuming a timer a later beat has taken over.
//!
//! A zero repeat interval (`--every 0`) is rejected at parse/config time: it
//! would make [`newt_core::timer::advance_repeat`] spin forever once the job
//! is due. See `timer_cli::every_zero_is_rejected_cleanly`.

use std::path::{Path, PathBuf};

use clap::Subcommand;

use newt_core::timer::{AckOutcome, SystemClock, TimerStore};

#[derive(Subcommand, Debug)]
pub enum TimerCmd {
    /// Schedule a wake-up: re-enter the agent with <prompt> after <duration>.
    /// Use `--every` to re-arm on an interval ("watch a pipeline every 5m").
    Schedule {
        /// Human duration until first fire: `30s`, `5m`, `2h` (bare = seconds).
        duration: String,
        /// The prompt to fire back at the agent when the timer elapses.
        prompt: String,
        /// Re-arm on this interval after each fire (same syntax as <duration>).
        /// Must be greater than zero — a zero repeat would never advance.
        #[arg(long)]
        every: Option<String>,
        /// Write to this dir instead of the resolved newt config dir.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// List scheduled timers, soonest first.
    List {
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Drain due timers. Without `--run` this is a NON-MUTATING emit: it
    /// prints each due timer as `PROM\t<id>\t<prompt>` and leaves the queue
    /// alone, so the host must `newt timer dismiss <id>` once its own solve
    /// succeeds — otherwise every beat re-emits the same due prompt. With
    /// `--run` it re-enters the agent with each due prompt through the
    /// `newt solve` entry point (in-process) and manages the queue for you
    /// (claim → execute → acknowledge); that is the supported wake-up path.
    /// See the module docs.
    Fire {
        /// Re-enter the agent with each due prompt via `newt solve` (in-process)
        /// instead of printing `PROM\t…` lines. Runs the safe, confined lane a
        /// plain `newt solve` defaults to. Uses select/claim → execute →
        /// acknowledge semantics: a due timer is claimed, driven through
        /// solve, and only removed/advanced on success; a failed solve leaves
        /// the timer pending/retryable and stops the beat so later due timers
        /// are never silently lost.
        #[arg(long)]
        run: bool,
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Cancel a timer by id (or unambiguous id prefix).
    Dismiss {
        id: String,
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
}

/// Dispatch `newt timer <cmd>`. `config` is the global `--config` file,
/// threaded to `fire --run` so a host scheduler's backend selection reaches
/// the solve run the same way `newt solve --config` would.
pub async fn run(cmd: TimerCmd, config: Option<&Path>) -> anyhow::Result<i32> {
    match cmd {
        TimerCmd::Schedule {
            duration,
            prompt,
            every,
            dir,
        } => {
            let after = parse_duration(&duration)?;
            let repeat = every.as_deref().map(parse_duration).transpose()?;
            // Argument validation: reject a zero repeat before touching the
            // store — `advance_repeat` would spin forever once it is due.
            if let Some(0) = repeat {
                anyhow::bail!("--every must be greater than zero (got 0)");
            }
            let store = TimerStore::open(dir.as_deref())?;
            let clock = SystemClock;
            // Capture the scheduling CWD: the beat that fires this timer is a
            // cron/launchd process whose own CWD is unrelated, so execution
            // context must travel with the timer rather than be inferred.
            let workspace = std::env::current_dir().ok();
            let timer = store.schedule(after, &prompt, &clock, repeat, workspace)?;
            println!("scheduled {} (fires in {duration})", timer.id);
            Ok(0)
        }
        TimerCmd::List { dir } => {
            let store = TimerStore::open(dir.as_deref())?;
            for t in store.list()? {
                let rep = t
                    .repeat_secs
                    .map_or_else(|| "one-shot".to_string(), |s| format!("every {s}s"));
                println!("{}\t{}\t{}", t.id, t.fire_at, rep);
            }
            Ok(0)
        }
        TimerCmd::Fire { run, dir } => {
            let store = TimerStore::open(dir.as_deref())?;
            let clock = SystemClock;
            if !run {
                // Bare emit — non-mutating. The host owns the downstream solve
                // and must `dismiss` after it succeeds (or use `--run` for the
                // managed lifecycle). A due timer is NOT consumed merely for
                // being emitted.
                for t in store.due(&clock)? {
                    // The id is part of the record: the documented external
                    // lane ends in `newt timer dismiss <id>`, which is
                    // impossible if the emit never names the timer.
                    println!("PROM\t{}\t{}", t.id, encode_prompt_field(&t.prompt));
                }
                return Ok(0);
            }
            // `--run`: select/claim → execute → acknowledge, one timer at a
            // time. Stop at the first failed solve so later due timers are
            // never silently lost — they remain pending (unclaimed) for the
            // next beat. The failed timer's claim is released and it stays
            // pending/retryable.
            loop {
                let Some(timer) = store.claim_next_due(&clock)? else {
                    return Ok(0); // nothing due/claimable
                };
                let Some(token) = timer.claim_token.clone() else {
                    anyhow::bail!("claimed timer {} carries no claim token", timer.id);
                };
                let code = fire_solve(&timer.prompt, timer.workspace.as_deref(), config).await?;
                match store.acknowledge(&timer.id, &token, code == 0, &clock)? {
                    AckOutcome::Applied => {}
                    // Our claim went stale and another beat took the timer over
                    // mid-solve. Do not retry it here — the owner will ack it.
                    other => {
                        eprintln!(
                            "timer {}: claim no longer held ({other:?}); leaving it to its owner",
                            timer.id
                        );
                        return Ok(if code == 0 { 0 } else { code });
                    }
                }
                if code != 0 {
                    return Ok(code);
                }
            }
        }
        TimerCmd::Dismiss { id, dir } => {
            let store = TimerStore::open(dir.as_deref())?;
            if store.dismiss(&id)? {
                println!("dismissed {id}");
                Ok(0)
            } else {
                eprintln!("no timer uniquely matches '{id}'");
                Ok(1)
            }
        }
    }
}

/// Write `prompt` to a transient instruction file and drive one headless
/// solve against the current workspace via [`crate::solve::run`] — the same
/// entry point `newt solve --instruction-file` uses. The file is removed when
/// the guard drops, so the wake-up never leaves instruction litter behind.
async fn fire_solve(
    prompt: &str,
    workspace: Option<&Path>,
    config: Option<&Path>,
) -> anyhow::Result<i32> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("newt-timer-{}-{nonce}.txt", std::process::id()));
    std::fs::write(&path, prompt)?;
    let _remove = TempRemove(path.clone());
    let args = crate::solve::SolveArgs {
        // The timer's own workspace, not the beat process's CWD.
        cwd: workspace.map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        instruction_file: path,
        profile: config.map(Path::to_path_buf),
        non_interactive: true,
        unsafe_host_exec: false,
        confined: false,
        events: None,
        max_rounds: None,
        context_window: None,
        model_digest: None,
    };
    crate::solve::run(args).await
}

/// Remove a transient instruction file when dropped.
struct TempRemove(PathBuf);
impl Drop for TempRemove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Parse a tiny duration grammar: `30s`, `5m`, `2h`, bare integer = seconds.
fn parse_duration(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (num, unit) = s.split_at(s.len() - 1);
    // Every unit scales with `checked_mul`: an unchecked `n * 60` wraps for
    // `n > u64::MAX/60`, which in release turned a far-future duration into a
    // near-immediate one (`307445734561825861m` → 44 seconds) and panicked in
    // debug. Overflow is a bad duration, not a silently rescheduled timer.
    let scale = |n: u64, mul: u64, what: &str| -> anyhow::Result<u64> {
        n.checked_mul(mul)
            .ok_or_else(|| anyhow::anyhow!("duration '{s}' overflows: too many {what}"))
    };
    match unit {
        "s" => num
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("bad seconds '{s}': {e}")),
        "m" => scale(
            num.parse::<u64>()
                .map_err(|e| anyhow::anyhow!("bad minutes '{s}': {e}"))?,
            60,
            "minutes",
        ),
        "h" => scale(
            num.parse::<u64>()
                .map_err(|e| anyhow::anyhow!("bad hours '{s}': {e}"))?,
            3600,
            "hours",
        ),
        // Bare integer (no unit) → seconds.
        _ => s
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("bad duration '{s}': {e}")),
    }
}

/// Encode one `PROM` record field so the line protocol stays one-record-per-line
/// and tab-delimited regardless of the prompt's contents.
///
/// A prompt is arbitrary agent-authored text: an unescaped newline would split
/// one record across several lines (and the continuation lines would not carry
/// the `PROM` tag), while an unescaped tab would shift the field boundaries.
/// Backslash is escaped first so the encoding is injective and decodable.
#[must_use]
pub fn encode_prompt_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        // bare = seconds
        assert_eq!(parse_duration("300").unwrap(), 300);
        // zero delay is allowed (fires now); only a zero *repeat* is rejected.
        assert_eq!(parse_duration("0").unwrap(), 0);
        assert_eq!(parse_duration("0s").unwrap(), 0);
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5x").is_err());
    }

    /// Regression (#1747): `--every 0` must fail cleanly at argument validation,
    /// before any timer is persisted. Validated before `TimerStore::open`, so
    /// this never touches the filesystem.
    #[tokio::test]
    async fn schedule_rejects_zero_repeat() {
        for bad in ["0", "0s", "0m", "0h"] {
            let cmd = TimerCmd::Schedule {
                duration: "5m".into(),
                prompt: "p".into(),
                every: Some(bad.into()),
                dir: None,
            };
            let err = run(cmd, None).await.unwrap_err().to_string();
            assert!(
                err.contains("greater than zero"),
                "--every {bad} should be rejected: {err}"
            );
        }
        // A non-zero repeat parses and reaches the store layer (which fails on
        // a path where a file blocks directory creation — cross-platform; no
        // platform-specific magic paths).
        let blocker = std::env::temp_dir().join(format!(
            "newt-test-block-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::write(&blocker, "x").unwrap();
        let cmd = TimerCmd::Schedule {
            duration: "5m".into(),
            prompt: "p".into(),
            every: Some("60".into()),
            dir: Some(blocker.join("sub")),
        };
        assert!(run(cmd, None).await.is_err());
        let _ = std::fs::remove_file(&blocker);
    }

    /// Regression (#1747): every duration unit scales with checked arithmetic.
    /// `n * 60` wrapped for `n > u64::MAX/60` — in release
    /// `307445734561825861m` became a 44-second timer instead of erroring, and
    /// in debug it panicked. Overflow must be a clean parse error.
    #[test]
    fn parse_duration_rejects_overflowing_units() {
        // u64::MAX / 60 == 307445734561825860, so +1 overflows the minute scale.
        let m = format!("{}m", (u64::MAX / 60) + 1);
        let err = parse_duration(&m).unwrap_err().to_string();
        assert!(err.contains("overflows"), "{err}");

        let h = format!("{}h", (u64::MAX / 3600) + 1);
        let err = parse_duration(&h).unwrap_err().to_string();
        assert!(err.contains("overflows"), "{err}");

        // The largest in-range values still parse exactly.
        assert_eq!(
            parse_duration(&format!("{}m", u64::MAX / 60)).unwrap(),
            (u64::MAX / 60) * 60
        );
        assert_eq!(
            parse_duration(&format!("{}h", u64::MAX / 3600)).unwrap(),
            (u64::MAX / 3600) * 3600
        );
        // A bare-seconds value never scales, so it is unaffected.
        assert_eq!(parse_duration(&u64::MAX.to_string()).unwrap(), u64::MAX);
    }

    /// Regression (#1747): the `PROM` line protocol survives an arbitrary
    /// agent-authored prompt. An unescaped newline split one timer across
    /// several lines (continuations carrying no `PROM` tag); an unescaped tab
    /// shifted the field boundaries.
    #[test]
    fn prom_field_encoding_keeps_one_timer_on_one_line() {
        let nasty = "line one\nline two\tcolumn\r\nback\\slash";
        let encoded = encode_prompt_field(nasty);
        assert!(
            !encoded.contains('\n') && !encoded.contains('\t') && !encoded.contains('\r'),
            "encoded field must stay on one line and one column: {encoded:?}"
        );
        assert_eq!(encoded, r"line one\nline two\tcolumn\r\nback\\slash");

        // A full record still splits into exactly the three intended fields.
        let record = format!("PROM\t{}\t{}", "tm_1_1000", encoded);
        let fields: Vec<&str> = record.split('\t').collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[1], "tm_1_1000");
        assert_eq!(record.lines().count(), 1);

        // Encoding is injective: distinct prompts never collide.
        assert_ne!(encode_prompt_field(r"a\nb"), encode_prompt_field("a\nb"));
    }
}
