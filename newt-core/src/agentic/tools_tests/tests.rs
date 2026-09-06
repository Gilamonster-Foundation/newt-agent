use super::*;
use crate::agentic::NoMcp;

#[derive(Default)]
pub(super) struct RecordingLiveOutput {
    pub(in crate::agentic::tools) events: std::sync::Mutex<Vec<String>>,
}

impl crate::agentic::LiveToolOutput for RecordingLiveOutput {
    fn start(&self, _generation: u64) {
        self.events.lock().unwrap().push("start".into());
    }

    fn write(&self, _generation: u64, stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
        self.events
            .lock()
            .unwrap()
            .push(format!("{stream:?}:{}", String::from_utf8_lossy(chunk)));
    }

    fn finish(&self, _generation: u64) {
        self.events.lock().unwrap().push("finish".into());
    }

    fn abandon(&self, _generation: u64) {
        self.events.lock().unwrap().push("abandon".into());
    }
}

#[cfg(test)]
#[path = "helper_validation.rs"]
mod validation;

#[cfg(test)]
#[path = "helper_find.rs"]
mod find;

#[cfg(test)]
#[path = "helper_live_output.rs"]
mod live_output;

#[cfg(all(test, windows, feature = "windows-appcontainer"))]
#[path = "helper_windows_appcontainer.rs"]
mod windows_appcontainer;

#[cfg(test)]
#[path = "helper_phantom_discovery.rs"]
mod phantom_discovery;

#[cfg(test)]
#[path = "helper_output_bounds.rs"]
mod output_bounds;

#[cfg(test)]
#[path = "helper_catalog.rs"]
mod catalog;

#[cfg(test)]
#[path = "helper_catalog_authority.rs"]
mod catalog_authority;

#[cfg(test)]
#[path = "helper_lifecycle.rs"]
mod lifecycle;

#[cfg(test)]
#[path = "helper_shell_routing.rs"]
mod shell_routing;

#[cfg(test)]
#[path = "helper_shell_authority.rs"]
mod shell_authority;

#[cfg(test)]
#[path = "helper_file_provenance.rs"]
mod file_provenance;

#[cfg(test)]
#[path = "helper_collaborators.rs"]
mod collaborators;
