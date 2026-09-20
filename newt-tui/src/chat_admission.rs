//! Actual final model-input admission, after command interception.
use super::{begin_model_prompt, ModelInputOrigin, PromptIngress};

pub(super) enum ModelPromptAdmission {
    Refused(String),
    Accepted(Box<newt_core::TurnPromptContext>),
}

pub(super) fn admit_model_prompt(
    tab: &crate::tabs::TabState,
    ingress: PromptIngress<'_>,
    title: &str,
    persona: Option<&str>,
    raw: &[u8],
    model: &[u8],
    origin: &ModelInputOrigin,
) -> anyhow::Result<ModelPromptAdmission> {
    if let Some(reason) = crate::tab_switch::degraded_turn_refusal(tab.pin_degraded.as_ref()) {
        return Ok(ModelPromptAdmission::Refused(reason));
    }
    // Preserve existing ordering: admissible input announces Working before
    // durable ingress, including when insertion subsequently returns an error.
    newt_core::lifecycle::emit(newt_core::lifecycle::LifecycleEvent::TurnStarted);
    begin_model_prompt(
        ingress,
        tab.conversation_id(),
        title,
        persona,
        raw,
        model,
        origin,
    )
    .map(|context| ModelPromptAdmission::Accepted(Box::new(context)))
}
