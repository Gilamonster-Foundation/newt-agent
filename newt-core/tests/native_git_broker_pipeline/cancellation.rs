//! #2686: cancellation must leave HEAD detached until the execution lease's
//! shutdown path finishes. This does not prove escaped descendants have exited.

use super::native::{init_worktree, real_git_output, session_caveats};
use agent_toolchain::{git_caveats::GitCaveats, native_git::CommitPolicy};
use newt_core::{agentic::GitTool, Caveats, NoMcp};
use std::sync::{mpsc, Arc, Mutex};

struct HookHandshake {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl CommitPolicy for HookHandshake {
    fn finalize_message(&self, message: &str) -> Result<String, String> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            entered.send(()).unwrap();
            // The real Git hook is waiting for this host broker callback.
            // BrokerRuntime::drop must join it before releasing the lease.
            self.release.lock().unwrap().recv().unwrap();
        }
        Ok(message.to_owned())
    }
    fn signing_required(&self) -> bool {
        false
    }
    fn sign_commit(&self, _: &[u8]) -> Result<String, String> {
        Err("signing is not exercised".into())
    }
    fn committed(&self) {}
}

struct GitFixture(Arc<HookHandshake>);
impl GitTool for GitFixture {
    fn native_commit_policy(&self) -> Option<Arc<dyn CommitPolicy>> {
        Some(self.0.clone())
    }
    fn dispatch(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &GitCaveats,
        _: &Caveats,
    ) -> Result<String, String> {
        Err("embedded Git is not exercised".into())
    }
}

// Release even when an assertion fails, so a red regression cannot strand the
// broker callback and block the Tokio runtime's blocking-pool shutdown.
struct Release(mpsc::Sender<()>);
impl Drop for Release {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

pub(super) async fn run() {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let root = tempfile::tempdir().unwrap();
    let (main, wt) = init_worktree(root.path());
    let session = session_caveats(&wt);
    let old_tip = real_git_output(&main, &["rev-parse", "refs/heads/task"]);
    let admin = real_git_output(&wt, &["rev-parse", "--absolute-git-dir"]);
    let head = std::path::Path::new(&admin).join("HEAD");

    // Kernel notifications acknowledge recovery's atomic HEAD rename. No
    // sleeps, polling intervals, deadlines, or elapsed-time assertions.
    // SAFETY: flags are valid; the returned fd is transferred to File once.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    assert!(fd >= 0, "{}", std::io::Error::last_os_error());
    let mut changes = unsafe { std::fs::File::from_raw_fd(fd) };
    let admin_c =
        std::ffi::CString::new(std::path::Path::new(&admin).as_os_str().as_bytes()).unwrap();
    // SAFETY: admin_c is a live NUL-terminated path, fd owns an inotify instance.
    assert!(unsafe { libc::inotify_add_watch(fd, admin_c.as_ptr(), libc::IN_MOVED_TO) } >= 0);

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release = Release(release_tx);
    let git_tool = GitFixture(Arc::new(HookHandshake {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    }));
    let args = serde_json::json!({"command": "git commit -q --allow-empty -m cancelled"});
    let workspace = wt.to_string_lossy();
    let mut mcp = NoMcp;
    // Losing this select branch DROPS the real dispatch future. Merely pinning
    // it outside select would retain the guard and mask the original bug.
    tokio::select! {
        out = newt_core::execute_tool(
            "run_command", &args, &workspace, false, 200, &session, &mut mcp,
            None, None, None, None, None, None, Some(&git_tool),
            None, None, None, None, None, None,
        ) => panic!("dispatch finished before hook handshake: {out}"),
        entered = entered_rx => entered.expect("the live Git hook must enter the broker"),
    }
    let head_while_shutdown_blocked = std::fs::read_to_string(&head).unwrap();
    drop(release);
    assert_eq!(
        head_while_shutdown_blocked.trim(),
        old_tip,
        "cancelling the waiter must not reattach HEAD while the lease shutdown callback is blocked"
    );

    // A queued detach notification may arrive first. Each blocking read waits
    // for a real rename; the final symbolic HEAD acknowledges recovery itself.
    let mut events = [0_u8; 4096];
    while std::fs::read_to_string(&head).unwrap().trim() != "ref: refs/heads/task" {
        assert!(changes.read(&mut events).unwrap() > 0);
    }
    assert_eq!(
        real_git_output(&main, &["rev-parse", "refs/heads/task"]),
        old_tip,
        "cancelled dispatch must never publish"
    );
    println!("CANCELLED_COMMIT_LEASE_ORDERING_CONFIRMED");
}
