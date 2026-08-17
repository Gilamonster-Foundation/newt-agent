//! **Shadow-OCAP flight recorder** (#1176) — capture the authority an
//! `--full-access` session actually uses, without enforcing it.
//!
//! Under `--full-access` the OCAP attenuations are lifted and commands run
//! unconfined — fast, unblocked, but you learn nothing about what authority
//! the agent *needed*. This module turns that blind path into an **observe
//! path**: every unconfined operation emits a [`ShadowCaveat`] — the concrete
//! capability + target a confined run would have gated on. The command still
//! runs with full authority; the recorder only *notes* what a leash would have
//! required.
//!
//! Two outputs from one capture (see #1176):
//! 1. the **policy-gap catalog** — the set of caveats the session used, to
//!    diff against existing policy (needed − in-policy = what's unaccounted
//!    for). Never an auto-grant; observation only reports gaps.
//! 2. a **bridle repro fixture** — the real command/construct that ran, so we
//!    can replay it into agent-bridle's test suite and make the confined
//!    engine handle it correctly. Every `--full-access` run pays down its own
//!    risk: it becomes both a policy proposal and a test case.
//!
//! This module is PURE: it builds and serializes records. The session wires
//! the append (fs) at the dispatch boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The capability axis a shadow caveat covers — mirrors the OCAP axes so a
/// capture folds directly into the policy schema (agent-bridle#221).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowAxis {
    Exec,
    FsRead,
    FsWrite,
    Net,
}

impl ShadowAxis {
    /// The policy-file capability-class label this axis maps to.
    pub fn class(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::FsRead | Self::FsWrite => "fs",
            Self::Net => "net",
        }
    }
}

/// One observed authority: a capability + the concrete target a confined run
/// would have gated on, plus the raw command that produced it (the repro
/// fixture) and how many times it was seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowCaveat {
    pub axis: ShadowAxis,
    /// The gated target: an exec program name, an fs path, or a net host.
    pub target: String,
    /// The raw command that produced this observation — the bridle repro
    /// fixture. Kept verbatim so the exact construct can be replayed.
    pub command: String,
    /// How many times this (axis, target) was observed this session.
    pub count: u64,
}

/// A session's accumulated capture, deduplicated by (axis, target). The map
/// key keeps the catalog compact while `count`/`command` preserve frequency
/// and a representative fixture.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FlightCapture {
    /// (axis, target) → the merged caveat.
    #[serde(with = "caveat_map")]
    pub caveats: BTreeMap<(ShadowAxis, String), ShadowCaveat>,
}

impl FlightCapture {
    /// Record that an unconfined command exercised `axis` on `target`. Merges
    /// by (axis, target): bumps the count, keeps the first command as the
    /// fixture. Empty targets are ignored (nothing to gate).
    pub fn observe(&mut self, axis: ShadowAxis, target: &str, command: &str) {
        let target = target.trim();
        if target.is_empty() {
            return;
        }
        let key = (axis, target.to_string());
        self.caveats
            .entry(key)
            .and_modify(|c| c.count += 1)
            .or_insert_with(|| ShadowCaveat {
                axis,
                target: target.to_string(),
                command: command.to_string(),
                count: 1,
            });
    }

    /// Observe every authority a raw shell command would have needed. MVP:
    /// the exec leading token (the program a confined run gates on). fs/net
    /// extraction (redirect targets, connect hosts) lands in follow-ups —
    /// logged here as the single most valuable axis first.
    pub fn observe_command(&mut self, command: &str) {
        if let Some(program) = exec_program(command) {
            self.observe(ShadowAxis::Exec, program, command);
        }
    }

    /// The policy GAP: the observed caveats whose (class, target) is not
    /// already covered by `in_policy`. This is the set-diff the operator
    /// reviews — never an auto-grant. `in_policy` is a set of (class, target)
    /// pairs the current policy already allows.
    pub fn gaps<'a>(
        &'a self,
        in_policy: &std::collections::HashSet<(String, String)>,
    ) -> Vec<&'a ShadowCaveat> {
        self.caveats
            .values()
            .filter(|c| !in_policy.contains(&(c.axis.class().to_string(), c.target.clone())))
            .collect()
    }

    /// Serialize to pretty JSON for the session capture file.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| format!("flight capture serialize: {e}"))
    }

    /// Parse a capture file back.
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| format!("flight capture parse: {e}"))
    }
}

/// The exec program a confined run would gate on: the basename of the leading
/// token. Mirrors the leading-token rule the exec leash uses. `None` for an
/// empty command or one whose first token is empty.
pub fn exec_program(command: &str) -> Option<&str> {
    let first = command.split_ascii_whitespace().next()?;
    Some(
        first
            .rsplit(['/', '\\'])
            .find(|p| !p.is_empty())
            .unwrap_or(first),
    )
}

/// Serde helper: a `BTreeMap<(ShadowAxis, String), _>` isn't a JSON object key,
/// so serialize as a flat list of caveats and rebuild the map on read.
mod caveat_map {
    use super::{ShadowAxis, ShadowCaveat};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<(ShadowAxis, String), ShadowCaveat>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        map.values().collect::<Vec<_>>().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<(ShadowAxis, String), ShadowCaveat>, D::Error> {
        let list = Vec::<ShadowCaveat>::deserialize(d)?;
        Ok(list
            .into_iter()
            .map(|c| ((c.axis, c.target.clone()), c))
            .collect())
    }
}

/// The env var the CLI sets (to a capture-file path) when `--full-access` runs
/// with recording on. Absent = recording off (the hook is a zero-cost no-op).
pub const CAPTURE_PATH_ENV: &str = "NEWT_FLIGHT_RECORDER";

/// Hook for the unconfined **exec** path (#1176): when a capture path is set,
/// append this command's would-be caveats as JSONL (one [`ShadowCaveat`] per
/// line, dedup/fold happens at read time). A no-op — never an error to the
/// caller — when recording is off: the flight recorder must not perturb the
/// session it observes.
pub fn log_unconfined(command: &str) {
    let mut cap = FlightCapture::default();
    cap.observe_command(command);
    append_capture(&cap);
}

/// Hook for the unconfined **fs / net** paths (#1176): record ONE observed
/// caveat — an explicit `axis` + `target` (an fs path, or a net host) plus the
/// `command`/tool-name repro fixture — the same append-only way as
/// [`log_unconfined`]. Under `--full-access` the fs fence and net leash are
/// `top()`, so these operations run unconfined and would otherwise leave no
/// trace of the authority a real leash would have gated on. A no-op when
/// recording is off or the target is empty.
pub fn log_observed(axis: ShadowAxis, target: &str, command: &str) {
    let mut cap = FlightCapture::default();
    cap.observe(axis, target, command);
    append_capture(&cap);
}

/// Append every caveat in `cap` to the armed capture file as JSONL (one
/// [`ShadowCaveat`] per line). Append-only, so concurrent dispatches never race
/// a read-modify-write; every error is swallowed — the recorder never perturbs
/// the session. A no-op when recording is off (env unset) or `cap` is empty.
fn append_capture(cap: &FlightCapture) {
    if cap.caveats.is_empty() {
        return;
    }
    let Some(path) = std::env::var_os(CAPTURE_PATH_ENV) else {
        return;
    };
    use std::io::Write as _;
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        for c in cap.caveats.values() {
            if let Ok(line) = serde_json::to_string(c) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// Read a JSONL capture file back into a folded [`FlightCapture`] (dedup by
/// axis+target, summing counts) — the input to `newt ocap propose` (#1176).
pub fn read_capture_jsonl(text: &str) -> FlightCapture {
    let mut cap = FlightCapture::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(c) = serde_json::from_str::<ShadowCaveat>(line) {
            for _ in 0..c.count.max(1) {
                cap.observe(c.axis, &c.target, &c.command);
            }
        }
    }
    cap
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// RAII guard that restores an env var to its prior value on drop, so a
    /// test can mutate the env without leaking state to siblings even if the
    /// body panics.
    struct EnvRestore {
        key: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn exec_program_takes_the_basename_of_the_leading_token() {
        assert_eq!(exec_program("cargo build --release"), Some("cargo"));
        assert_eq!(exec_program("/usr/bin/npm ci"), Some("npm"));
        assert_eq!(exec_program("  git   push  "), Some("git"));
        assert_eq!(exec_program(""), None);
    }

    #[test]
    fn observe_dedupes_by_axis_target_and_counts() {
        let mut cap = FlightCapture::default();
        cap.observe_command("cargo build");
        cap.observe_command("cargo test");
        cap.observe_command("git push origin main");
        assert_eq!(cap.caveats.len(), 2, "cargo merged, git distinct");
        let cargo = &cap.caveats[&(ShadowAxis::Exec, "cargo".into())];
        assert_eq!(cargo.count, 2);
        assert_eq!(
            cargo.command, "cargo build",
            "first command kept as fixture"
        );
        // Empty targets are ignored.
        cap.observe(ShadowAxis::Exec, "  ", "   ");
        assert_eq!(cap.caveats.len(), 2);
    }

    #[test]
    fn gaps_are_the_set_diff_never_an_auto_grant() {
        // #1176: observation reports what's UNACCOUNTED FOR against policy;
        // it never grants. `cargo` is already policy; `rm` is a gap.
        let mut cap = FlightCapture::default();
        cap.observe_command("cargo build");
        cap.observe_command("rm -rf target");
        let mut in_policy = HashSet::new();
        in_policy.insert(("exec".to_string(), "cargo".to_string()));
        let gaps = cap.gaps(&in_policy);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].target, "rm");
        assert_eq!(
            gaps[0].command, "rm -rf target",
            "gap carries the repro fixture"
        );
    }

    #[test]
    fn jsonl_capture_folds_by_axis_target() {
        // The append-only JSONL from log_unconfined folds back to the same
        // deduped catalog (counts summed across lines).
        let lines = concat!(
            "{\"axis\":\"exec\",\"target\":\"cargo\",\"command\":\"cargo build\",\"count\":1}\n",
            "{\"axis\":\"exec\",\"target\":\"cargo\",\"command\":\"cargo test\",\"count\":1}\n",
            "\n",
            "{\"axis\":\"net\",\"target\":\"crates.io\",\"command\":\"curl x\",\"count\":1}\n",
        );
        let cap = read_capture_jsonl(lines);
        assert_eq!(cap.caveats.len(), 2);
        assert_eq!(cap.caveats[&(ShadowAxis::Exec, "cargo".into())].count, 2);
    }

    #[test]
    fn fs_and_net_axes_fold_and_map_to_the_policy_classes() {
        // #1176 fs/net axis: the fs (read/write) and net arms emit these
        // ShadowCaveats via log_observed. Prove the JSONL they append folds
        // back and maps to the policy capability classes the consumer reads —
        // fs_read/fs_write → "fs", net → "net" — with counts summed.
        let lines = concat!(
            "{\"axis\":\"fs_read\",\"target\":\"/ws/src\",\"command\":\"read_file\",\"count\":1}\n",
            "{\"axis\":\"fs_read\",\"target\":\"/ws/src\",\"command\":\"list_dir\",\"count\":1}\n",
            "{\"axis\":\"fs_write\",\"target\":\"/ws/out\",\"command\":\"write_file\",\"count\":1}\n",
            "{\"axis\":\"net\",\"target\":\"docs.rs\",\"command\":\"https://docs.rs/x\",\"count\":1}\n",
        );
        let cap = read_capture_jsonl(lines);
        // read+read on the same path fold to one caveat (count 2); write is a
        // distinct axis+target; net is its own.
        assert_eq!(cap.caveats.len(), 3);
        let read = &cap.caveats[&(ShadowAxis::FsRead, "/ws/src".into())];
        assert_eq!(read.count, 2);
        assert_eq!(read.axis.class(), "fs");
        assert_eq!(
            cap.caveats[&(ShadowAxis::FsWrite, "/ws/out".into())]
                .axis
                .class(),
            "fs"
        );
        assert_eq!(
            cap.caveats[&(ShadowAxis::Net, "docs.rs".into())]
                .axis
                .class(),
            "net"
        );
    }

    /// When recording is off (capture env unset), `log_observed` must be a
    /// no-op that never panics — it's called on every fs/net op under
    /// `--full-access`. The ambient harness shell may set
    /// `NEWT_FLIGHT_RECORDER` (it does under `--full-access`), so this test
    /// isolates the env (save/unset/restore via an RAII guard) rather than
    /// asserting the ambient env is unset. Serial-gated so the env mutation
    /// can't race a parallel test that also reads the same var.
    #[test]
    #[serial_test::serial(flight_recorder_env)]
    fn log_observed_is_a_no_op_when_recording_is_off() {
        let _restore = EnvRestore {
            key: CAPTURE_PATH_ENV,
            saved: std::env::var_os(CAPTURE_PATH_ENV),
        };
        std::env::remove_var(CAPTURE_PATH_ENV);
        assert!(std::env::var_os(CAPTURE_PATH_ENV).is_none());
        log_observed(ShadowAxis::FsRead, "/ws/x", "read_file");
        log_observed(ShadowAxis::Net, "example.com", "https://example.com");
    }

    #[test]
    fn capture_round_trips_through_json() {
        let mut cap = FlightCapture::default();
        cap.observe_command("cargo build");
        cap.observe_command("cargo build");
        cap.observe(ShadowAxis::Net, "crates.io", "curl https://crates.io");
        let json = cap.to_json().unwrap();
        let back = FlightCapture::parse(&json).unwrap();
        assert_eq!(cap, back);
        assert_eq!(back.caveats[&(ShadowAxis::Exec, "cargo".into())].count, 2);
        assert_eq!(
            back.caveats[&(ShadowAxis::Net, "crates.io".into())]
                .axis
                .class(),
            "net"
        );
    }
}
