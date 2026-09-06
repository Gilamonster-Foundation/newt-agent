use super::*;
use std::fs;

/// A real store on tempdirs, mirroring the conversation-command tests.
/// Returns the dirs so they outlive the store.
fn recall_test_store() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    newt_core::ConversationStore,
) {
    let state = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();
    let store = newt_core::ConversationStore::new(state.path(), workspace.path(), 100).unwrap();
    (state, workspace, store)
}

/// Everything a resume needs, on temp dirs — the borrow-heavy parts stay
/// in each test (ConversationCommandContext borrows them all mutably).
fn resume_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    newt_core::ConversationStore,
    PersonaStore,
) {
    let state = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();
    let store = newt_core::ConversationStore::new(state.path(), workspace.path(), 100).unwrap();
    let persona_dir = state.path().join("personas");
    fs::create_dir_all(&persona_dir).unwrap();
    (state, workspace, store, PersonaStore::new(persona_dir))
}

// Shared fixtures live here because more than one family calls them;
// every single-family helper moved with its family.
#[cfg(test)]
#[path = "skills_integration/auto_resume.rs"]
mod auto_resume;
#[cfg(test)]
#[path = "skills_integration/compress.rs"]
mod compress;
#[cfg(test)]
#[path = "skills_integration/conversation_ops.rs"]
mod conversation_ops;
#[cfg(test)]
#[path = "skills_integration/ephemeral.rs"]
mod ephemeral;
#[cfg(test)]
#[path = "skills_integration/help_corpus.rs"]
mod help_corpus;
#[cfg(test)]
#[path = "skills_integration/persona.rs"]
mod persona;
#[cfg(test)]
#[path = "skills_integration/recall_render.rs"]
mod recall_render;
#[cfg(test)]
#[path = "skills_integration/restore.rs"]
mod restore;
#[cfg(test)]
#[path = "skills_integration/resume_by_name.rs"]
mod resume_by_name;
#[cfg(test)]
#[path = "skills_integration/roadmap.rs"]
mod roadmap;
#[cfg(test)]
#[path = "skills_integration/save_paths.rs"]
mod save_paths;
#[cfg(test)]
#[path = "skills_integration/system_prompt.rs"]
mod system_prompt;
