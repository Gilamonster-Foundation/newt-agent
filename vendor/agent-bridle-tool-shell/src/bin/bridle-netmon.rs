//! `bridle-netmon` — a live monitor for the agent's egress audit trail
//! (#124, ADR 0016).
//!
//! The macOS egress proxy is the confined agent's sole network chokepoint; when
//! the `BRIDLE_NET_AUDIT` setting names a file, it appends one JSON line per
//! proxied connection ([`NetAuditEvent`]). This binary tails that file (or stdin)
//! and renders a per-host table — connections, allowed / **denied** / errored,
//! bytes up/down, and last-seen — refreshing live, like `iptraf`/`nmap`.
//!
//! Std-only (no TUI framework). Meaning is carried in **text**, never colour
//! alone: a denied host is flagged with a `! DENIED` marker, not a hue.
//!
//! Usage:
//!   BRIDLE_NET_AUDIT=/tmp/egress.jsonl <run the agent>   # producer
//!   bridle-netmon /tmp/egress.jsonl                       # this monitor
//!   tail -F /tmp/egress.jsonl | bridle-netmon             # or pipe via stdin

use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_bridle_tool_shell::{NetAuditEvent, NetDecision};

fn main() -> io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("-h") | Some("--help") => {
            eprintln!(
                "bridle-netmon — live egress audit monitor (#124, ADR 0016)\n\
                 usage: bridle-netmon [AUDIT_FILE]   (default: stdin)\n\
                 the AUDIT_FILE is what `BRIDLE_NET_AUDIT` names when running the agent."
            );
            Ok(())
        }
        None | Some("-") => run_stdin(),
        Some(path) => run_file(path),
    }
}

/// Tail `path` (poll for appended lines, survive truncation/rotation), redrawing
/// every refresh tick so relative "last-seen" ages stay current.
fn run_file(path: &str) -> io::Result<()> {
    let mut agg = Agg::default();
    let mut pos: u64 = 0;
    // Byte buffer (not String): a polled window can end mid-multibyte-char (an IDN
    // host) or on a mid-write boundary, so `read_to_string` would return
    // `InvalidData` and `?` would kill the live monitor (#138). We keep raw bytes,
    // split on `\n`, and lossy-decode complete lines — a split char is preserved in
    // `leftover` until the rest of the line arrives.
    let mut leftover: Vec<u8> = Vec::new();
    loop {
        if let Ok(mut f) = std::fs::File::open(path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                // Truncated or rotated — start over.
                pos = 0;
                leftover.clear();
                agg = Agg::default();
            }
            if len > pos {
                f.seek(SeekFrom::Start(pos))?;
                let mut buf: Vec<u8> = Vec::new();
                f.take(len - pos).read_to_end(&mut buf)?;
                pos = len;
                leftover.extend_from_slice(&buf);
                drain_lines(&mut leftover, &mut agg);
            }
        }
        draw(&agg)?;
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Read newline-delimited events from stdin (e.g. `tail -F file | bridle-netmon`),
/// redrawing on each line.
fn run_stdin() -> io::Result<()> {
    let stdin = io::stdin();
    let mut agg = Agg::default();
    // Split on raw bytes + lossy-decode (not `.lines()`, which returns `Err` on
    // invalid UTF-8 and would kill the monitor on an IDN host / partial char, #138).
    for chunk in stdin.lock().split(b'\n') {
        let bytes = chunk?;
        let line = String::from_utf8_lossy(&bytes);
        if let Ok(ev) = serde_json::from_str::<NetAuditEvent>(line.trim()) {
            agg.ingest(ev);
        }
        draw(&agg)?;
    }
    Ok(())
}

/// Split complete lines out of `buf` (keeping any trailing partial line) and fold
/// each parseable event into `agg`.
fn drain_lines(buf: &mut Vec<u8>, agg: &mut Agg) {
    while let Some(i) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=i).collect();
        // Lossy: never fail on invalid/partial UTF-8 (a split multibyte char at a
        // window boundary stays in `buf` until its line completes, so a *complete*
        // line here is well-formed; the lossy decode is belt-and-suspenders).
        let line = String::from_utf8_lossy(&line);
        if let Ok(ev) = serde_json::from_str::<NetAuditEvent>(line.trim()) {
            agg.ingest(ev);
        }
    }
}

fn draw(agg: &Agg) -> io::Result<()> {
    let mut out = io::stdout().lock();
    // Home cursor + clear to end of screen (avoids the flicker of a full 2J wipe).
    out.write_all(b"\x1b[H\x1b[J")?;
    out.write_all(agg.render(now_ms()).as_bytes())?;
    out.flush()
}

/// Per-host rollup of the egress audit trail.
#[derive(Default)]
struct Agg {
    hosts: BTreeMap<String, Stat>,
    total: u64,
}

#[derive(Default, Clone)]
struct Stat {
    conns: u64,
    allowed: u64,
    denied: u64,
    error: u64,
    up: u64,
    down: u64,
    last_ms: u64,
}

impl Agg {
    fn ingest(&mut self, e: NetAuditEvent) {
        self.total += 1;
        let s = self.hosts.entry(e.host).or_default();
        s.conns += 1;
        match e.decision {
            NetDecision::Allowed => s.allowed += 1,
            NetDecision::Denied => s.denied += 1,
            NetDecision::Error => s.error += 1,
        }
        s.up += e.bytes_up;
        s.down += e.bytes_down;
        s.last_ms = s.last_ms.max(e.ts_ms);
    }

    /// Render the whole screen as a string. Rows are sorted most-recent-first so a
    /// live view surfaces current activity; a host with any denial is flagged in
    /// text (`! DENIED`) so the signal never depends on colour.
    fn render(&self, now: u64) -> String {
        let mut rows: Vec<(&String, &Stat)> = self.hosts.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1.last_ms)); // most-recent first

        let mut out = String::new();
        out.push_str(&format!(
            "agent egress monitor — {} connection(s) across {} host(s)\n",
            self.total,
            self.hosts.len()
        ));
        out.push_str(&format!(
            "{:<30} {:>5} {:>4} {:>4} {:>4} {:>9} {:>9} {:>6}\n",
            "HOST", "CONNS", "OK", "DENY", "ERR", "UP", "DOWN", "LAST"
        ));
        out.push_str(&format!("{}\n", "-".repeat(78)));
        for (host, s) in rows {
            let flag = if s.denied > 0 {
                "  ! DENIED"
            } else if s.error > 0 {
                "  ~ error"
            } else {
                ""
            };
            out.push_str(&format!(
                "{:<30} {:>5} {:>4} {:>4} {:>4} {:>9} {:>9} {:>6}{}\n",
                trunc(host, 30),
                s.conns,
                s.allowed,
                s.denied,
                s.error,
                fmt_bytes(s.up),
                fmt_bytes(s.down),
                ago(now, s.last_ms),
                flag,
            ));
        }
        if self.hosts.is_empty() {
            out.push_str("(waiting for egress activity…)\n");
        }
        out.push_str("\n(Ctrl-C to quit · ! DENIED = allow-list refusal / exfil attempt)\n");
        out
    }
}

/// Unix-epoch milliseconds now (0 before the epoch; never panics).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A compact "time since" for the LAST column.
fn ago(now: u64, then: u64) -> String {
    if then == 0 {
        return "-".to_string();
    }
    let secs = now.saturating_sub(then) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

/// Human-readable byte count (B/KB/MB/GB), 1 decimal above bytes.
fn fmt_bytes(n: u64) -> String {
    const UNIT: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNIT.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", UNIT[i])
    }
}

/// Truncate a host to `w` columns with an ellipsis, so a long name can't skew the
/// table.
fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        let keep: String = s.chars().take(w.saturating_sub(1)).collect();
        format!("{keep}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridle_tool_shell::NetKind;

    fn ev(host: &str, decision: NetDecision, up: u64, down: u64, ts: u64) -> NetAuditEvent {
        NetAuditEvent {
            ts_ms: ts,
            host: host.to_string(),
            port: 443,
            kind: NetKind::Connect,
            decision,
            bytes_up: up,
            bytes_down: down,
            dur_ms: 1,
        }
    }

    #[test]
    fn ingest_rolls_up_per_host() {
        let mut a = Agg::default();
        a.ingest(ev("api.x", NetDecision::Allowed, 100, 900, 10));
        a.ingest(ev("api.x", NetDecision::Allowed, 0, 100, 20));
        a.ingest(ev("evil.x", NetDecision::Denied, 0, 0, 30));
        assert_eq!(a.total, 3);
        let x = &a.hosts["api.x"];
        assert_eq!(
            (x.conns, x.allowed, x.up, x.down, x.last_ms),
            (2, 2, 100, 1000, 20)
        );
        assert_eq!(a.hosts["evil.x"].denied, 1);
    }

    #[test]
    fn render_flags_denials_in_text_not_colour() {
        let mut a = Agg::default();
        a.ingest(ev("evil.x", NetDecision::Denied, 0, 0, 30));
        let screen = a.render(30_000);
        assert!(screen.contains("evil.x"));
        assert!(
            screen.contains("! DENIED"),
            "a denial must be flagged in text: {screen}"
        );
        // No ANSI colour SGR codes — meaning never depends on hue.
        assert!(
            !screen.contains("\x1b["),
            "render must not embed colour codes: {screen:?}"
        );
    }

    #[test]
    fn drain_lines_keeps_a_partial_trailing_line() {
        let mut buf: Vec<u8> = Vec::new();
        let mut a = Agg::default();
        let full = serde_json::to_string(&ev("h", NetDecision::Allowed, 1, 2, 5)).unwrap();
        buf.extend_from_slice(full.as_bytes());
        buf.push(b'\n');
        buf.extend_from_slice(b"{\"partial\":"); // incomplete line, must be retained
        drain_lines(&mut buf, &mut a);
        assert_eq!(a.total, 1, "the complete line was ingested");
        assert_eq!(
            buf, b"{\"partial\":",
            "the partial line is kept for next read"
        );
    }

    /// #138 (HIGH): a window ending mid-multibyte-char must not crash the tailer.
    /// A complete IDN-host line is ingested; the split byte is retained (byte-based
    /// buffering), never a UTF-8 decode failure.
    #[test]
    fn drain_lines_tolerates_split_multibyte_char() {
        let mut buf: Vec<u8> = Vec::new();
        let mut a = Agg::default();
        let full = serde_json::to_string(&ev("café.test", NetDecision::Allowed, 1, 2, 5)).unwrap();
        buf.extend_from_slice(full.as_bytes());
        buf.push(b'\n');
        buf.push(0xC3); // first byte of a 2-byte UTF-8 char, split at the window edge
        drain_lines(&mut buf, &mut a); // must NOT panic / error on the partial byte
        assert_eq!(a.total, 1, "the complete IDN-host line was ingested");
        assert_eq!(
            buf,
            vec![0xC3],
            "the split byte is retained for the next read"
        );
    }

    #[test]
    fn fmt_bytes_and_ago() {
        assert_eq!(fmt_bytes(0), "0B");
        assert_eq!(fmt_bytes(512), "512B");
        assert_eq!(fmt_bytes(1536), "1.5KB");
        assert_eq!(ago(10_000, 7_000), "3s");
        assert_eq!(ago(130_000, 10_000), "2m");
        assert_eq!(ago(1000, 0), "-");
    }
}
