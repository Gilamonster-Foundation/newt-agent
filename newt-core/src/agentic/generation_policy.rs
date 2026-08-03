use crate::model_card::{ChatCompletionsCapability, ReasoningReplayScope};
use crate::role_profile::Cognition;

/// Backend-neutral generation choices resolved once per Chat Completions turn.
/// Request serialization projects these values only onto fields the endpoint
/// explicitly declared it accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct GenerationPolicy {
    pub(crate) thinking: Option<bool>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) parallel_tool_calls: Option<bool>,
    pub(crate) reasoning_replay_scope: ReasoningReplayScope,
    pub(crate) chat_template_kwargs: bool,
    pub(crate) one_bounded_reasoning_continuation: bool,
}

impl GenerationPolicy {
    /// Resolve the operator's cognition dial against endpoint capability data.
    /// The numeric table is the initial local-agent policy from the Nemotron
    /// qualification plan; no model display name participates in selection.
    #[must_use]
    pub(crate) fn resolve(
        cognition: Option<Cognition>,
        capability: ChatCompletionsCapability,
        reasoning_replay_scope: ReasoningReplayScope,
    ) -> Self {
        let mut policy = Self {
            parallel_tool_calls: capability.parallel_tool_calls,
            reasoning_replay_scope,
            chat_template_kwargs: capability.chat_template_kwargs == Some(true),
            one_bounded_reasoning_continuation: capability.bounded_reasoning_continuation
                == Some(true),
            ..Self::default()
        };

        if capability.cognition != Some(true) {
            return policy;
        }
        let Some(cognition) = cognition else {
            return policy;
        };

        let (thinking, max_output_tokens, temperature, top_p) = match cognition {
            Cognition::Glancing => (false, 2_048, 0.0, 1.0),
            Cognition::Pondering => (true, 4_096, 0.6, 0.95),
            Cognition::Deliberating => (true, 10_000, 0.6, 0.95),
            Cognition::Contemplating => (true, 16_000, 0.6, 0.95),
        };
        policy.thinking = Some(thinking);
        policy.max_output_tokens = Some(max_output_tokens);
        policy.temperature = Some(temperature);
        policy.top_p = Some(top_p);
        policy
    }

    /// Add only explicitly resolved Chat Completions fields. An empty policy
    /// is a strict no-op for unknown compatible endpoints.
    pub(crate) fn apply_to_chat_completions_body(self, body: &mut serde_json::Value) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        if let Some(max_tokens) = self.max_output_tokens {
            object.insert("max_tokens".into(), serde_json::json!(max_tokens));
        }
        if let Some(temperature) = self.temperature {
            object.insert("temperature".into(), serde_json::json!(temperature));
        }
        if let Some(top_p) = self.top_p {
            object.insert("top_p".into(), serde_json::json!(top_p));
        }
        if object.contains_key("tools") {
            if let Some(parallel_tool_calls) = self.parallel_tool_calls {
                object.insert(
                    "parallel_tool_calls".into(),
                    serde_json::json!(parallel_tool_calls),
                );
            }
        }
        if self.chat_template_kwargs {
            if let Some(thinking) = self.thinking {
                object.insert(
                    "chat_template_kwargs".into(),
                    serde_json::json!({
                        "enable_thinking": thinking,
                        "truncate_history_thinking": true,
                    }),
                );
            }
        }
    }

    /// One automatic reasoning continuation is safe only when the endpoint
    /// opted in, current-turn reasoning can be replayed, and the ordinary
    /// round budget can pay for the extra request.
    #[must_use]
    pub(crate) fn allows_reasoning_continuation(
        self,
        already_attempted: bool,
        has_round_budget: bool,
    ) -> bool {
        !already_attempted
            && self.one_bounded_reasoning_continuation
            && self.reasoning_replay_scope != ReasoningReplayScope::Never
            && has_round_budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_card::{ChatCompletionsCapability, ReasoningReplayScope};
    use crate::role_profile::Cognition;

    fn local_capability() -> ChatCompletionsCapability {
        ChatCompletionsCapability {
            cognition: Some(true),
            chat_template_kwargs: Some(true),
            parallel_tool_calls: Some(false),
            bounded_reasoning_continuation: Some(true),
        }
    }

    #[test]
    fn unknown_endpoint_keeps_the_chat_request_policy_empty() {
        let policy = GenerationPolicy::resolve(
            Some(Cognition::Deliberating),
            ChatCompletionsCapability::default(),
            ReasoningReplayScope::Never,
        );

        assert_eq!(policy, GenerationPolicy::default());
    }

    #[test]
    fn cognition_levels_resolve_to_the_initial_local_generation_table() {
        let cases = [
            (Cognition::Glancing, false, 2_048, 0.0, 1.0),
            (Cognition::Pondering, true, 4_096, 0.6, 0.95),
            (Cognition::Deliberating, true, 10_000, 0.6, 0.95),
            (Cognition::Contemplating, true, 16_000, 0.6, 0.95),
        ];

        for (cognition, thinking, max_tokens, temperature, top_p) in cases {
            let policy = GenerationPolicy::resolve(
                Some(cognition),
                local_capability(),
                ReasoningReplayScope::CurrentUserTurn,
            );
            assert_eq!(policy.thinking, Some(thinking), "{cognition}");
            assert_eq!(policy.max_output_tokens, Some(max_tokens), "{cognition}");
            assert_eq!(policy.temperature, Some(temperature), "{cognition}");
            assert_eq!(policy.top_p, Some(top_p), "{cognition}");
            assert_eq!(policy.parallel_tool_calls, Some(false), "{cognition}");
            assert!(policy.chat_template_kwargs, "{cognition}");
            assert!(policy.one_bounded_reasoning_continuation, "{cognition}");
            assert_eq!(
                policy.reasoning_replay_scope,
                ReasoningReplayScope::CurrentUserTurn,
                "{cognition}"
            );
        }
    }

    #[test]
    fn endpoint_extensions_can_opt_in_without_enabling_cognition_projection() {
        let policy = GenerationPolicy::resolve(
            Some(Cognition::Contemplating),
            ChatCompletionsCapability {
                cognition: Some(false),
                chat_template_kwargs: Some(true),
                parallel_tool_calls: Some(false),
                bounded_reasoning_continuation: Some(true),
            },
            ReasoningReplayScope::CurrentUserTurn,
        );

        assert_eq!(policy.thinking, None);
        assert_eq!(policy.max_output_tokens, None);
        assert_eq!(policy.temperature, None);
        assert_eq!(policy.top_p, None);
        assert_eq!(policy.parallel_tool_calls, Some(false));
        assert!(policy.chat_template_kwargs);
        assert!(policy.one_bounded_reasoning_continuation);
    }

    #[test]
    fn opted_in_policy_projects_to_chat_completions_fields() {
        let policy = GenerationPolicy::resolve(
            Some(Cognition::Deliberating),
            local_capability(),
            ReasoningReplayScope::CurrentUserTurn,
        );
        let mut body = serde_json::json!({
            "model": "served-model",
            "messages": [],
            "tools": [],
            "tool_choice": "auto",
            "stream": false
        });

        policy.apply_to_chat_completions_body(&mut body);

        assert_eq!(body["max_tokens"], 10_000);
        assert_eq!(body["temperature"], 0.6);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(
            body["chat_template_kwargs"]["truncate_history_thinking"],
            true
        );
    }

    #[test]
    fn default_policy_leaves_the_chat_completions_body_byte_for_byte_equal() {
        let mut body = serde_json::json!({
            "model": "strict-endpoint",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false
        });
        let before = body.clone();

        GenerationPolicy::default().apply_to_chat_completions_body(&mut body);

        assert_eq!(body, before);
    }

    #[test]
    fn tools_disabled_completion_omits_parallel_tool_calls() {
        let policy = GenerationPolicy::resolve(
            Some(Cognition::Pondering),
            local_capability(),
            ReasoningReplayScope::CurrentUserTurn,
        );
        let mut body = serde_json::json!({
            "model": "served-model",
            "messages": [],
            "stream": false
        });

        policy.apply_to_chat_completions_body(&mut body);

        assert!(body.get("parallel_tool_calls").is_none());
        assert_eq!(body["max_tokens"], 4_096);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn bounded_reasoning_continuation_requires_capability_replay_and_round_budget() {
        let enabled = GenerationPolicy::resolve(
            None,
            local_capability(),
            ReasoningReplayScope::CurrentUserTurn,
        );
        assert!(enabled.allows_reasoning_continuation(false, true));
        assert!(!enabled.allows_reasoning_continuation(true, true));
        assert!(!enabled.allows_reasoning_continuation(false, false));

        let no_replay =
            GenerationPolicy::resolve(None, local_capability(), ReasoningReplayScope::Never);
        assert!(!no_replay.allows_reasoning_continuation(false, true));

        let no_capability = GenerationPolicy::resolve(
            None,
            ChatCompletionsCapability::default(),
            ReasoningReplayScope::CurrentUserTurn,
        );
        assert!(!no_capability.allows_reasoning_continuation(false, true));
    }
}
