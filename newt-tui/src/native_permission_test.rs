//! Explicit test-only adapter: each gate has the production turn lifetime.
use crate::permissions::{production_danger_table, PermissionPromptState, PromptPermissionGate};
use newt_core::{Caveats, PermissionAction, PermissionGate};
use std::sync::atomic::AtomicBool;

#[derive(Default)]
pub struct Session(PermissionPromptState);

impl Session {
    pub fn turn<'a>(
        &'a mut self,
        base: Caveats,
        conversation: &str,
        cancel: &'a AtomicBool,
        mut answer: impl FnMut() -> PermissionAction + 'a,
    ) -> impl PermissionGate + 'a {
        PromptPermissionGate {
            pending_command_retries: Default::default(),
            state: &mut self.0,
            base,
            key_path: None,
            conversation_id: conversation.into(),
            log_path: None,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            delegation: None,
            danger: production_danger_table(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: std::time::Duration::from_secs(2),
            cancel: Some(cancel),
            exit: None,
            ask_surface: None,
            #[cfg(feature = "rich-tui")]
            open_panel: None,
            ask_human:
                move |_: &newt_core::tty::PromptWindow,
                      _: &newt_core::interaction_surface::SurfaceInteraction| {
                    answer()
                },
        }
    }
}
