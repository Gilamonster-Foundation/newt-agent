//! The test probe `ToolCollaborators::command_probe` carries into a `run_command` dispatch.
//! It stands in for the confined shell and for the host gh read broker and counts each route
//! it is sent down. `Probe::run`, the gh read half, is in `gh_read_tests.rs`.
use std::sync::atomic::AtomicUsize;

pub(crate) struct Probe {
    pub(super) host: AtomicUsize,
    pub(super) confined: AtomicUsize,
    pub(super) remote: String,
    pub(super) calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    pub(super) output: String,
    pub(super) exit_code: i32,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            host: AtomicUsize::new(0),
            confined: AtomicUsize::new(0),
            remote: "github.com/owner/repo".into(),
            calls: Default::default(),
            output: "issue 1".into(),
            exit_code: 0,
        }
    }
}
