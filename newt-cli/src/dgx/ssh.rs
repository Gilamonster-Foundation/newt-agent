use crate::{dgx_pull, dgx_status};

/// How the pull touches the node: real SSH, or a captured dry-run.
///
/// The trait lets tests substitute a recording fake so no real SSH ever runs.
pub(super) trait SshExec {
    /// Run `command` on `user@host` (optional `port`), streaming output to the
    /// child's inherited stderr. Returns Ok on a zero exit.
    fn run(&self, user: &str, host: &str, port: Option<u16>, command: &str) -> anyhow::Result<()>;
}

/// Default executor: spawns the real `ssh` binary.
pub(super) struct RealSsh;

impl SshExec for RealSsh {
    fn run(&self, user: &str, host: &str, port: Option<u16>, command: &str) -> anyhow::Result<()> {
        let argv = dgx_pull::ssh_argv(user, host, port, command);
        let (prog, rest) = argv.split_first().expect("ssh argv non-empty");
        let status = std::process::Command::new(prog)
            .args(rest)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to spawn ssh: {e}"))?;
        if !status.success() {
            anyhow::bail!("ssh command failed: {status}");
        }
        Ok(())
    }
}

/// Real SSH capture for `dgx status` budget gathering: spawns `ssh` and returns
/// its captured stdout. The pure parsing/budget logic lives behind the
/// [`dgx_status::SshCapture`] seam (unit-tested with canned stdout), so this thin
/// wrapper is the only un-unit-tested edge — it mirrors [`RealSsh`].
pub(super) struct RealSshCapture;

impl dgx_status::SshCapture for RealSshCapture {
    fn capture(
        &self,
        user: &str,
        host: &str,
        port: Option<u16>,
        command: &str,
    ) -> anyhow::Result<String> {
        let argv = dgx_pull::ssh_argv(user, host, port, command);
        let (prog, rest) = argv.split_first().expect("ssh argv non-empty");
        let out = std::process::Command::new(prog)
            .args(rest)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to spawn ssh: {e}"))?;
        if !out.status.success() {
            anyhow::bail!("ssh command failed: {}", out.status);
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// `free -b` awk column for total RAM (`MemTotal`).
pub(super) const MEM_TOTAL_AWK: &str = "$2";
/// `free -b` awk column for available RAM (`MemAvailable`).
pub(super) const MEM_AVAILABLE_AWK: &str = "$7";

/// The remote command that prints one memory figure from `free -b` (pure).
pub(super) fn node_mem_probe(awk_field: &str) -> String {
    format!("free -b | awk '/Mem:/{{print {awk_field}}}'")
}

/// Detect node RAM (bytes) via `free -b`, selecting the awk column. Best-effort:
/// `None` if SSH or parsing fails.
fn detect_node_mem_col(user: &str, host: &str, port: Option<u16>, awk_field: &str) -> Option<u64> {
    let argv = dgx_pull::ssh_argv(user, host, port, &node_mem_probe(awk_field));
    let (prog, rest) = argv.split_first()?;
    let out = std::process::Command::new(prog).args(rest).output().ok()?;
    if !out.status.success() {
        return None;
    }
    dgx_pull::parse_free_bytes(&String::from_utf8_lossy(&out.stdout))
}

/// Detect total node RAM (`MemTotal`). Used by the Ollama `pull` fit pre-flight,
/// whose semantics deliberately stay unchanged in this step.
pub(super) fn detect_node_mem(user: &str, host: &str, port: Option<u16>) -> Option<u64> {
    detect_node_mem_col(user, host, port, MEM_TOTAL_AWK)
}

/// Available node RAM (`MemAvailable`, column 7). vLLM's fit budget must net out
/// memory the *other* engine (Ollama) already holds resident — see
/// [`crate::dgx_vllm::vllm_fit_check`] — so it sizes against available, not
/// total RAM.
pub(super) fn detect_node_mem_available(user: &str, host: &str, port: Option<u16>) -> Option<u64> {
    detect_node_mem_col(user, host, port, MEM_AVAILABLE_AWK)
}
