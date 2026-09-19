//! Captured verification settings and the strict pursuit obligation (#2451).
use super::super::prompt_intake::PromptDisposition;

#[derive(Debug, Clone, Copy)]
pub(crate) struct VerificationSettings {
    pub enabled: bool,
    pub outcomes: bool,
}

std::thread_local! {
    static SETTINGS: std::cell::Cell<Option<VerificationSettings>> = const { std::cell::Cell::new(None) };
}

impl VerificationSettings {
    pub(crate) fn capture() -> Self {
        SETTINGS.with(std::cell::Cell::get).unwrap_or_else(|| Self {
            enabled: !std::env::var("NEWT_SELF_VERIFY").is_ok_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "off" | "false"
                )
            }),
            outcomes: super::outcomes_switch(std::env::var("NEWT_VERIFY_OUTCOMES").ok().as_deref()),
        })
    }
}

/// Current-thread capture, nested and restored just like the psyche dials.
pub(crate) struct ScopedVerificationSettings {
    previous: Option<VerificationSettings>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

pub(crate) fn scoped_verification_settings(
    settings: VerificationSettings,
) -> ScopedVerificationSettings {
    ScopedVerificationSettings {
        previous: SETTINGS.with(|slot| slot.replace(Some(settings))),
        _thread_bound: std::marker::PhantomData,
    }
}

impl Drop for ScopedVerificationSettings {
    fn drop(&mut self) {
        let _ = SETTINGS.try_with(|slot| slot.set(self.previous));
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VerificationPolicy {
    pub required: bool,
    pub enabled: bool,
    pub result_aware: bool,
}

impl VerificationPolicy {
    pub(crate) fn capture(disposition: PromptDisposition, outcomes: bool) -> Self {
        let required = disposition == PromptDisposition::Act
            && crate::tenacity::effective_tenacity().requires_verification();
        let enabled = required || VerificationSettings::capture().enabled;
        Self {
            required,
            enabled,
            result_aware: required || (enabled && outcomes),
        }
    }

    pub(crate) fn gate_on(self, advisory: bool) -> bool {
        self.required || advisory
    }

    pub(crate) fn receipt(self, gate_present: bool) -> serde_json::Value {
        let mut receipt =
            super::verification_receipt(gate_present, self.enabled, self.result_aware);
        if self.required {
            receipt["required_by_tenacity"] = serde_json::json!(true);
        }
        receipt
    }
}
