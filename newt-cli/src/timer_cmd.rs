//! `newt timer` — the self-scheduled wake-up surface.
//!
//! `schedule` records a deferred prompt; `list` shows the queue; `fire` drains
//! due timers and prints each prompt on a `PROM\t<prompt>` line so a host
//! scheduler — cron, launchd, or a shell loop — can pipe them into a headless
//! `newt solve` / `newt code` run; `dismiss` cancels a watch. The agent is not
//! resident between turns, so the **host** owns the clock and re-enters the
//! model here. Reuses the pure decision core in [`newt_core::timer`].

use std::path::PathBuf;

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
    /// Drain due timers: print each due prompt on a `PROM\t<prompt>` line and
    /// update the queue (dismiss one-shots, advance `--every` timers). No
    /// output when nothing is due — safe to call from a cron every minute.
    Fire {
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

/// Dispatch `newt timer <cmd>`.
pub fn run(cmd: TimerCmd) -> anyhow::Result<i32> {
    match cmd {
        TimerCmd::Schedule {
            duration,
            prompt,
            every,
            dir,
        } => {
            let store = TimerStore::open(dir.as_deref())?;
            let after = parse_duration(&duration)?;
            let repeat = every.as_deref().map(parse_duration).transpose()?;
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
        TimerCmd::Fire { dir } => {
            let store = TimerStore::open(dir.as_deref())?;
            let clock = SystemClock;
            let due = store.fire_due(&clock)?;
            for t in &due {
                println!("PROM\t{}", t.prompt);
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
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5x").is_err());
    }
}
