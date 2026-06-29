//! `newt dgx status` memory-budget introspection (issue #709, PR1 — read-only).
//!
//! Pure parsers (`/proc/meminfo` → bytes; `ps` output → workloads), the remote
//! probe commands, the budget model, and human/JSON rendering. The SSH read is
//! an injectable seam ([`SshCapture`]) so [`gather_budget`] is unit-tested with
//! canned stdout — never a live connection. Pattern copied from `dgx_pull.rs`
//! (pure, fully mocked, fs-free).
//!
//! Security: nothing here names a host/IP/GPU. The node address is supplied by
//! the caller from the operator's local `[dgx]` config at runtime; tests use
//! fabricated sample text only.

use crate::dgx_pull::bytes_to_gib;

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// A recognized local inference engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    /// Ollama (`ollama serve` / runner subprocess).
    Ollama,
    /// vLLM (`vllm serve` / `python -m vllm...`).
    Vllm,
}

impl WorkloadKind {
    /// Stable lowercase token for display / JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
        }
    }
}

/// A running inference process detected on the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    /// Which engine this process belongs to.
    pub kind: WorkloadKind,
    /// Resident set size in bytes (best-effort; RSS does not capture VRAM on a
    /// unified-memory node, so treat it as a floor, not the full footprint).
    pub rss_bytes: u64,
    /// The process command line (truncated by `ps` to its argv).
    pub command: String,
}

/// A node memory-budget snapshot (issue #709 §1):
/// `{ total, used, available, running_workloads[] }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemBudget {
    /// Total physical memory (bytes) — `/proc/meminfo` `MemTotal`.
    pub total_bytes: u64,
    /// Available memory (bytes) — `/proc/meminfo` `MemAvailable`.
    pub available_bytes: u64,
    /// Detected inference workloads (ollama / vllm).
    pub workloads: Vec<Workload>,
}

impl MemBudget {
    /// Used memory (bytes): `total - available`, saturating.
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
}

// ---------------------------------------------------------------------------
// Remote probe commands (pure)
// ---------------------------------------------------------------------------

/// Remote command that dumps `/proc/meminfo` (parsed by [`parse_meminfo`]).
pub fn meminfo_command() -> &'static str {
    "cat /proc/meminfo"
}

/// Remote command listing every process's RSS (KiB) + full argv, suitable for
/// [`parse_ps_workloads`]. `rss=` / `args=` suppress the header.
pub fn ps_command() -> &'static str {
    "ps -eo rss=,args="
}

// ---------------------------------------------------------------------------
// Parsers (pure)
// ---------------------------------------------------------------------------

/// Read one `Key:<int> kB` field from `/proc/meminfo` and return it in **bytes**
/// (the file reports KiB). `None` if the key is absent or the value unparsable.
fn meminfo_field_bytes(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        // /proc/meminfo lines are `Key:` with no space before the colon, so we
        // require the colon immediately after the key to avoid prefix collisions.
        if let Some(rest) = line.trim_start().strip_prefix(key) {
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            let value: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(value.saturating_mul(1024));
        }
    }
    None
}

/// Parse `MemTotal` + `MemAvailable` from `/proc/meminfo` into `(total, available)`
/// **bytes**. `None` if either field is missing/unparsable.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let total = meminfo_field_bytes(text, "MemTotal")?;
    let available = meminfo_field_bytes(text, "MemAvailable")?;
    Some((total, available))
}

/// Keyword → kind table for recognizing inference processes by command line.
///
/// THREE-CS REFACTOR CANDIDATE: like the model registry, this hardcodes domain
/// knowledge (which process names are inference engines). It should later become
/// droppable config; kept inline now for a working detector.
const WORKLOAD_MATCHERS: &[(&str, WorkloadKind)] = &[
    ("vllm", WorkloadKind::Vllm),
    ("ollama", WorkloadKind::Ollama),
];

/// Classify a command line as an inference engine, if any (case-insensitive
/// substring match against [`WORKLOAD_MATCHERS`]). Best-effort.
fn classify_command(command: &str) -> Option<WorkloadKind> {
    let lc = command.to_ascii_lowercase();
    WORKLOAD_MATCHERS
        .iter()
        .find(|(kw, _)| lc.contains(kw))
        .map(|(_, kind)| *kind)
}

/// Parse `ps -eo rss=,args=` output into inference [`Workload`]s. Each line is
/// `<rss_kib> <command...>`; lines that are blank, malformed, or not an
/// inference engine are skipped (graceful — junk yields an empty list).
pub fn parse_ps_workloads(stdout: &str) -> Vec<Workload> {
    stdout.lines().filter_map(parse_ps_line).collect()
}

/// Parse one `ps` line into a [`Workload`], or `None` if it isn't one.
fn parse_ps_line(line: &str) -> Option<Workload> {
    let line = line.trim();
    let (rss_str, command) = line.split_once(char::is_whitespace)?;
    let rss_kib: u64 = rss_str.parse().ok()?;
    let command = command.trim();
    let kind = classify_command(command)?;
    Some(Workload {
        kind,
        rss_bytes: rss_kib.saturating_mul(1024),
        command: command.to_string(),
    })
}

// ---------------------------------------------------------------------------
// SSH capture seam + gather
// ---------------------------------------------------------------------------

/// Injection seam for the SSH read: returns the remote command's stdout. The
/// real impl spawns `ssh` (in `dgx.rs`); tests substitute canned stdout.
pub trait SshCapture {
    /// Run `command` on `user@host` (optional `port`) and return its stdout.
    fn capture(
        &self,
        user: &str,
        host: &str,
        port: Option<u16>,
        command: &str,
    ) -> anyhow::Result<String>;
}

/// Gather the node [`MemBudget`] over `ssh`: read `/proc/meminfo` + `ps`, parse.
/// The capture seam keeps this fully unit-testable with canned stdout.
pub fn gather_budget(
    ssh: &dyn SshCapture,
    user: &str,
    host: &str,
    port: Option<u16>,
) -> anyhow::Result<MemBudget> {
    let meminfo = ssh.capture(user, host, port, meminfo_command())?;
    let (total_bytes, available_bytes) = parse_meminfo(&meminfo).ok_or_else(|| {
        anyhow::anyhow!("could not parse MemTotal/MemAvailable from remote /proc/meminfo")
    })?;
    let ps_out = ssh.capture(user, host, port, ps_command())?;
    let workloads = parse_ps_workloads(&ps_out);
    Ok(MemBudget {
        total_bytes,
        available_bytes,
        workloads,
    })
}

// ---------------------------------------------------------------------------
// Rendering (pure)
// ---------------------------------------------------------------------------

/// Round to one decimal place (stable GiB rendering for JSON / tests).
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Human-readable budget rendering (the non-`--json` form).
pub fn render_budget_human(host: &str, budget: &MemBudget) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "DGX memory budget — {host}");
    let _ = writeln!(
        s,
        "  Total:      {:.1} GiB",
        bytes_to_gib(budget.total_bytes)
    );
    let _ = writeln!(
        s,
        "  Used:       {:.1} GiB",
        bytes_to_gib(budget.used_bytes())
    );
    let _ = writeln!(
        s,
        "  Available:  {:.1} GiB",
        bytes_to_gib(budget.available_bytes)
    );
    if budget.workloads.is_empty() {
        let _ = writeln!(s, "  Workloads:  (none detected)");
    } else {
        let _ = writeln!(s, "  Workloads:");
        for w in &budget.workloads {
            let _ = writeln!(
                s,
                "    {:<7} {:>6.1} GiB  {}",
                w.kind.as_str(),
                bytes_to_gib(w.rss_bytes),
                w.command
            );
        }
    }
    s
}

/// Structured budget as JSON (the `--json` form): the issue #709 §1 shape,
/// `{ total_gib, used_gib, available_gib, running_workloads[] }`, with raw byte
/// fields alongside for exactness.
pub fn budget_json(budget: &MemBudget) -> serde_json::Value {
    serde_json::json!({
        "total_gib": round1(bytes_to_gib(budget.total_bytes)),
        "used_gib": round1(bytes_to_gib(budget.used_bytes())),
        "available_gib": round1(bytes_to_gib(budget.available_bytes)),
        "total_bytes": budget.total_bytes,
        "used_bytes": budget.used_bytes(),
        "available_bytes": budget.available_bytes,
        "running_workloads": budget
            .workloads
            .iter()
            .map(|w| serde_json::json!({
                "kind": w.kind.as_str(),
                "rss_bytes": w.rss_bytes,
                "rss_gib": round1(bytes_to_gib(w.rss_bytes)),
                "command": w.command,
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dgx_pull::ssh_argv;

    // Fabricated /proc/meminfo: MemTotal 134217728 kB == 128.0 GiB exactly,
    // MemAvailable 100663296 kB == 96.0 GiB exactly (so used == 32.0 GiB).
    const SAMPLE_MEMINFO: &str = "\
MemTotal:       134217728 kB
MemFree:          1048576 kB
MemAvailable:   100663296 kB
Buffers:           524288 kB
Cached:          20971520 kB
";

    // Fabricated `ps -eo rss=,args=`: an ollama (8.0 GiB) + a vllm (32.0 GiB)
    // process, plus non-inference noise that must be ignored.
    const SAMPLE_PS: &str = "\
 8388608 /usr/local/bin/ollama serve
33554432 python3 -m vllm.entrypoints.openai.api_server --model Ornith-1.0-35B
    1234 /usr/sbin/sshd -D
     512 -bash
";

    // --- meminfo parser ------------------------------------------------

    #[test]
    fn parse_meminfo_reads_total_and_available_in_bytes() {
        let (total, available) = parse_meminfo(SAMPLE_MEMINFO).expect("parses");
        assert_eq!(total, 134_217_728u64 * 1024); // 128 GiB
        assert_eq!(available, 100_663_296u64 * 1024); // 96 GiB
        assert!((bytes_to_gib(total) - 128.0).abs() < 1e-9);
        assert!((bytes_to_gib(available) - 96.0).abs() < 1e-9);
    }

    #[test]
    fn parse_meminfo_missing_field_is_none() {
        // No MemAvailable line.
        let text = "MemTotal:       134217728 kB\nMemFree: 1048576 kB\n";
        assert!(parse_meminfo(text).is_none());
    }

    #[test]
    fn parse_meminfo_junk_is_none() {
        assert!(parse_meminfo("not memory info at all").is_none());
        assert!(parse_meminfo("").is_none());
    }

    #[test]
    fn meminfo_field_does_not_collide_on_prefix() {
        // "Mem" must not match "MemTotal" (the colon guard); the real key wins.
        assert!(meminfo_field_bytes(SAMPLE_MEMINFO, "Mem").is_none());
        assert_eq!(
            meminfo_field_bytes(SAMPLE_MEMINFO, "MemFree"),
            Some(1_048_576u64 * 1024)
        );
    }

    // --- ps parser -----------------------------------------------------

    #[test]
    fn parse_ps_extracts_inference_workloads_and_ignores_noise() {
        let workloads = parse_ps_workloads(SAMPLE_PS);
        assert_eq!(workloads.len(), 2);
        assert_eq!(workloads[0].kind, WorkloadKind::Ollama);
        assert_eq!(workloads[0].rss_bytes, 8_388_608u64 * 1024); // 8 GiB
        assert!(workloads[0].command.contains("ollama serve"));
        assert_eq!(workloads[1].kind, WorkloadKind::Vllm);
        assert_eq!(workloads[1].rss_bytes, 33_554_432u64 * 1024); // 32 GiB
    }

    #[test]
    fn parse_ps_junk_and_empty_are_graceful() {
        assert!(parse_ps_workloads("").is_empty());
        assert!(parse_ps_workloads("not_a_number ollama serve").is_empty());
        assert!(parse_ps_workloads("   \n\n").is_empty());
        // No inference engine on the line → skipped.
        assert!(parse_ps_workloads("12345 /usr/bin/bash").is_empty());
    }

    #[test]
    fn classify_command_matches_engines_case_insensitively() {
        assert_eq!(
            classify_command("/usr/bin/Ollama serve"),
            Some(WorkloadKind::Ollama)
        );
        assert_eq!(
            classify_command("python -m VLLM.entrypoints"),
            Some(WorkloadKind::Vllm)
        );
        assert_eq!(classify_command("/usr/bin/sshd -D"), None);
    }

    // --- remote command + ssh argv ------------------------------------

    #[test]
    fn probe_commands_are_stable() {
        assert_eq!(meminfo_command(), "cat /proc/meminfo");
        assert_eq!(ps_command(), "ps -eo rss=,args=");
    }

    #[test]
    fn ssh_argv_builds_the_expected_probe_command() {
        // Reuse the audited dgx_pull::ssh_argv builder; no live connection.
        assert_eq!(
            ssh_argv("bob", "node", None, meminfo_command()),
            vec!["ssh", "bob@node", "cat /proc/meminfo"]
        );
        assert_eq!(
            ssh_argv("bob", "node", Some(2222), ps_command()),
            vec!["ssh", "-p", "2222", "bob@node", "ps -eo rss=,args="]
        );
    }

    // --- gather_budget (mocked SSH seam) ------------------------------

    /// Canned-stdout capture: returns sample text by command, never touches SSH.
    struct FakeCapture {
        meminfo: String,
        ps: String,
    }

    impl SshCapture for FakeCapture {
        fn capture(
            &self,
            _user: &str,
            _host: &str,
            _port: Option<u16>,
            command: &str,
        ) -> anyhow::Result<String> {
            if command.contains("meminfo") {
                Ok(self.meminfo.clone())
            } else if command.starts_with("ps") {
                Ok(self.ps.clone())
            } else {
                anyhow::bail!("unexpected command: {command}")
            }
        }
    }

    #[test]
    fn gather_budget_parses_canned_stdout() {
        let fake = FakeCapture {
            meminfo: SAMPLE_MEMINFO.to_string(),
            ps: SAMPLE_PS.to_string(),
        };
        let budget = gather_budget(&fake, "u", "h", None).expect("gathers");
        assert_eq!(budget.total_bytes, 134_217_728u64 * 1024);
        assert_eq!(budget.available_bytes, 100_663_296u64 * 1024);
        assert_eq!(budget.used_bytes(), (134_217_728u64 - 100_663_296) * 1024); // 32 GiB
        assert_eq!(budget.workloads.len(), 2);
    }

    #[test]
    fn gather_budget_errors_on_unparsable_meminfo() {
        let fake = FakeCapture {
            meminfo: "garbage".to_string(),
            ps: SAMPLE_PS.to_string(),
        };
        let err = gather_budget(&fake, "u", "h", None).unwrap_err();
        assert!(err.to_string().contains("meminfo"), "{err}");
    }

    // --- rendering -----------------------------------------------------

    fn sample_budget() -> MemBudget {
        MemBudget {
            total_bytes: 134_217_728u64 * 1024,     // 128 GiB
            available_bytes: 100_663_296u64 * 1024, // 96 GiB
            workloads: parse_ps_workloads(SAMPLE_PS),
        }
    }

    #[test]
    fn render_human_shows_budget_and_workloads() {
        let out = render_budget_human("the-node", &sample_budget());
        assert!(out.contains("DGX memory budget — the-node"));
        assert!(out.contains("128.0 GiB"));
        assert!(out.contains("96.0 GiB"));
        assert!(out.contains("32.0 GiB")); // used
        assert!(out.contains("ollama"));
        assert!(out.contains("vllm"));
    }

    #[test]
    fn render_human_handles_no_workloads() {
        let budget = MemBudget {
            total_bytes: 134_217_728u64 * 1024,
            available_bytes: 134_217_728u64 * 1024,
            workloads: vec![],
        };
        let out = render_budget_human("n", &budget);
        assert!(out.contains("(none detected)"));
    }

    #[test]
    fn budget_json_has_the_issue_709_shape() {
        let v = budget_json(&sample_budget());
        assert_eq!(v["total_gib"], 128.0);
        assert_eq!(v["used_gib"], 32.0);
        assert_eq!(v["available_gib"], 96.0);
        assert_eq!(v["total_bytes"], 134_217_728u64 * 1024);
        let workloads = v["running_workloads"].as_array().expect("array");
        assert_eq!(workloads.len(), 2);
        assert_eq!(workloads[0]["kind"], "ollama");
        assert_eq!(workloads[0]["rss_gib"], 8.0);
        assert_eq!(workloads[1]["kind"], "vllm");
        assert_eq!(workloads[1]["rss_gib"], 32.0);
        // Round-trips through serde_json without panicking.
        let s = serde_json::to_string_pretty(&v).expect("serializes");
        assert!(s.contains("running_workloads"));
    }
}
