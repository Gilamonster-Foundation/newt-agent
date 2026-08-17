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
//! Without `--run`, `fire` prints each due prompt on a `PROM\t<prompt>` line
//! for a host that wants to feed a different entry point. That shell path must
//! use the supported `--instruction-file` interface — `newt solve` takes a
//! file, not a positional prompt:
//!
//! ```sh
//! newt timer fire | while IFS=$'\t' read -r _ prompt; do
//!   f=$(mktemp); printf '%s' "$prompt" > "$f"
//!   newt solve --instruction-file "$f"; rm -f "$f"
//! done
//! ```
//!
//! A zero repeat interval (`--every 0`) is rejected at parse/config time: it
//! would make [`newt_core::timer::advance_repeat`] spin forever once the job
//! is due. See `timer_cli::every_zero_is_rejected_cleanly`.

use std::path::{Path, PathBuf};

use clap::Subcommand;

use newt_core::timer::{SystemClock, TimerStore};

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
    /// Drain due timers. By default prints each due prompt on a
    /// `PROM\t<prompt>` line and updates the queue (dismiss one-shots, advance
    /// `--every` timers) — safe to call from a cron every minute, no output when
    /// nothing is due. With `--run`, instead re-enters the agent with each due
    /// prompt through the `newt solve` entry point (in-process) — the supported
    /// wake-up path; see the module docs.
    Fire {
        /// Re-enter the agent with each due prompt via `newt solve` (in-process)
        /// instead of printing `PROM\t…` lines. Runs the safe, confined lane a
        /// plain `newt solve` defaults to.
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
            let timer = store.schedule(after, &prompt, &clock, repeat)?;
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
            let due = store.fire_due(&clock)?;
            if due.is_empty() {
                return Ok(0);
            }
            if !run {
                for t in &due {
                    println!("PROM\t{}", t.prompt);
                }
                return Ok(0);
            }
            // `--run`: the supported wake-up path. Each due prompt drives a
            // real headless solve through the same `newt solve` entry point the
            // `Solve` command uses — the host owns the clock, the prompt
            // re-enters the model here. Stop on the first non-zero solve exit.
            for t in &due {
                let code = fire_solve(&t.prompt, config).await?;
                if code != 0 {
                    return Ok(code);
                }
            }
            Ok(0)
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
async fn fire_solve(prompt: &str, config: Option<&Path>) -> anyhow::Result<i32> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("newt-timer-{}-{nonce}.txt", std::process::id()));
    std::fs::write(&path, prompt)?;
    let _remove = TempRemove(path.clone());
    let args = crate::solve::SolveArgs {
        cwd: PathBuf::from("."),
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
    match unit {
        "s" => num
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("bad seconds '{s}': {e}")),
        "m" => num
            .parse::<u64>()
            .map(|n| n * 60)
            .map_err(|e| anyhow::anyhow!("bad minutes '{s}': {e}")),
        "h" => num
            .parse::<u64>()
            .map(|n| n * 3600)
            .map_err(|e| anyhow::anyhow!("bad hours '{s}': {e}")),
        // Bare integer (no unit) → seconds.
        _ => s
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("bad duration '{s}': {e}")),
    }
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
        // a non-existent dir here), proving the rejection above is zero-specific.
        let cmd = TimerCmd::Schedule {
            duration: "5m".into(),
            prompt: "p".into(),
            every: Some("60".into()),
            dir: Some(PathBuf::from("/dev/null/not-a-dir")),
        };
        assert!(run(cmd, None).await.is_err());
    }
}
