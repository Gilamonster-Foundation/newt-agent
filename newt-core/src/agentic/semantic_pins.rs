//! Adapt host-owned objective/target/report state before smart navigation.
use super::{claim_check::TurnClaims, prompt_read::PromptReadContext, smart_harness::SmartHarness};
use agent_harness::composition::{HostPin, PinClass};
use serde_json::{json, Value};

#[allow(clippy::too_many_arguments)]
pub(super) fn refresh(
    smart: Option<&SmartHarness>,
    messages: &mut Vec<Value>,
    prompt: PromptReadContext<'_>,
    selected: Option<&str>,
    claims: &TurnClaims<'_>,
    workspace: &str,
    read: &crate::Scope<String>,
) -> anyhow::Result<()> {
    let Some(smart) = smart else {
        return Ok(());
    };
    // These bytes come from the trusted producers, never a prefix classifier.
    let objective = prompt
        .active_receipt()
        .map(|p| p.root_prompt_id().to_string())
        .map(Ok)
        .unwrap_or_else(|| {
            content_addressable::canonical::to_canonical_dagcbor(&prompt.active_text()).map(
                |bytes| content_addressable::ContentId::from_canonical_bytes(&bytes).to_string(),
            )
        })?;
    let mut pins = vec![HostPin {
        class: PinClass::Objective,
        index: ensure_objective(messages, prompt.active_text()),
    }];
    if let Some(text) = selected {
        pins.push(HostPin {
            class: PinClass::SelectedTarget,
            index: ensure_message(messages, text),
        });
    }
    // Recompute after each completed tool batch: historical check receipts must
    // not become current-tree certification after a write or an adopted root.
    let report = claims.observed_report(workspace, read, "");
    let report = report
        .strip_suffix("\n## Model explanation\n\n")
        .unwrap_or(&report);
    let report =
        format!("[Current observed facts; historical checks retain their stated scope]\n{report}");
    pins.push(HostPin {
        class: PinClass::ObservedFacts,
        index: ensure_message(messages, &report),
    });
    smart.register_semantic_pins(&objective, messages, &pins)
}

fn ensure_message(messages: &mut Vec<Value>, text: &str) -> usize {
    let message = json!({"role":"user","content":text});
    if let Some(index) = messages.iter().position(|m| m == &message) {
        return index;
    }
    // Append so earlier registration indices stay stable. Explicit host origin
    // prevents report/target user envelopes from becoming operator anchors.
    messages.push(message);
    messages.len() - 1
}

// Recover an absent objective before conversational user messages so the
// recovered copy cannot displace a newer operator instruction as the anchor.
fn ensure_objective(messages: &mut Vec<Value>, text: &str) -> usize {
    let message = json!({"role":"user","content":text});
    if let Some(index) = messages.iter().position(|m| m == &message) {
        return index;
    }
    let index = messages
        .iter()
        .position(|m| !matches!(m["role"].as_str(), Some("system" | "developer")))
        .unwrap_or(messages.len());
    messages.insert(index, message);
    index
}
