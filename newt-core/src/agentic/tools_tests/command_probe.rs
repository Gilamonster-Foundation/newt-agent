//! The test probe `ToolCollaborators::command_probe` carries into a `run_command` dispatch.
//! It stands in for the confined shell and for the host gh read broker and counts each route
//! it is sent down. `Probe::run`, the gh read half, is in `gh_read_tests.rs`. That half and
//! the fields only it and the gh read broker read are built only with `confine` (#3160).
use std::sync::atomic::AtomicUsize;

pub(crate) struct Probe {
    #[cfg(feature = "confine")]
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
            #[cfg(feature = "confine")]
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
