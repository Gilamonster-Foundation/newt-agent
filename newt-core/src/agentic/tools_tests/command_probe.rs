//! The test probe `ToolCollaborators::command_probe` carries into a `run_command` dispatch.
//! It stands in for the confined shell and for the host gh read broker and counts each route
//! it is sent down. `Probe::run`, the gh read half, is in `gh_read_tests.rs`. That half and
//! the fields only it and the gh read broker read are built only with `confine` (#3160).
use std::sync::atomic::AtomicUsize;

pub(crate) struct Probe {
    /// Continue through production selection/refusal, replacing only I/O boundaries.
    pub(super) io_only: bool,
    pub(super) build: AtomicUsize,
    pub(super) exec_records: AtomicUsize,
    pub(super) host: AtomicUsize,
    pub(super) confined: AtomicUsize,
    #[cfg(feature = "confine")]
    pub(super) remote: String,
    #[cfg(feature = "confine")]
    pub(super) calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    #[cfg(feature = "confine")]
    pub(super) output: String,
    #[cfg(feature = "confine")]
    pub(super) exit_code: i32,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            io_only: false,
            build: AtomicUsize::new(0),
            exec_records: AtomicUsize::new(0),
            host: AtomicUsize::new(0),
            confined: AtomicUsize::new(0),
            #[cfg(feature = "confine")]
            remote: "github.com/owner/repo".into(),
            #[cfg(feature = "confine")]
            calls: Default::default(),
            #[cfg(feature = "confine")]
            output: "issue 1".into(),
            #[cfg(feature = "confine")]
            exit_code: 0,
        }
    }
}

// A task-local scope follows the async invocation without sharing a fake with another test.
// Route-arrival probes do not enter this scope and retain their original behavior.
tokio::task_local! {
    pub(super) static IO: std::sync::Arc<Probe>;
}

pub(super) fn with_io<T>(f: impl FnOnce(&Probe) -> T) -> Option<T> {
    IO.try_with(|probe| f(probe)).ok()
}

impl Probe {
    pub(super) fn ran(counter: &AtomicUsize) -> serde_json::Value {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        serde_json::json!({"exit_code": 0, "stdout": "mock shell", "stderr": ""})
    }
}
