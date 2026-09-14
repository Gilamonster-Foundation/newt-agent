//! Strict completion evidence layered on the shared SSE line parser.

use super::OpenAiStreamRound;
use anyhow::Context;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Default)]
pub(super) struct StrictResponse {
    event: Option<String>,
    id: Option<String>,
    model: Option<String>,
    usage: Option<Value>,
    finish_reason: Option<String>,
    calls: BTreeMap<u64, ToolFragments>,
    saw_choice: bool,
    problem: Option<anyhow::Error>,
    pub(super) provider_error: Option<anyhow::Error>,
}

impl StrictResponse {
    pub(super) fn event(&mut self, event: Option<&str>) {
        self.event = event.map(str::to_owned);
    }

    pub(super) fn problem(&mut self, message: &str) {
        self.problem
            .get_or_insert_with(|| anyhow::anyhow!("{message}"));
    }

    pub(super) fn observe(&mut self, frame: &Value) {
        if self.event.as_deref() == Some("error") || is_error(frame) {
            self.provider_error
                .get_or_insert_with(|| provider_error(frame));
        } else if let Err(error) = self.observe_chunk(frame) {
            self.problem.get_or_insert(error);
        }
    }

    fn observe_chunk(&mut self, frame: &Value) -> anyhow::Result<()> {
        stable_string(&mut self.id, &frame["id"], "response ID")?;
        stable_string(&mut self.model, &frame["model"], "response model")?;
        if !frame["usage"].is_null() {
            anyhow::ensure!(frame["usage"].is_object(), "invalid stream usage object");
            self.usage = Some(frame["usage"].clone());
        }
        let choices = frame["choices"]
            .as_array()
            .context("stream chunk has no choices array")?;
        anyhow::ensure!(
            choices.len() <= 1,
            "multiple streamed choices are unsupported"
        );
        let Some(choice) = choices.first() else {
            return Ok(());
        };
        anyhow::ensure!(
            choice["index"].is_null() || choice["index"].as_u64() == Some(0),
            "streamed choice index is not zero"
        );
        let delta = choice["delta"]
            .as_object()
            .context("streamed choice has no delta object")?;
        anyhow::ensure!(
            self.finish_reason.is_none() || delta.is_empty(),
            "stream changed a choice after its finish reason"
        );
        for key in ["content", "reasoning_content", "reasoning"] {
            anyhow::ensure!(
                choice["delta"][key].is_null() || choice["delta"][key].is_string(),
                "streamed text delta has an invalid shape"
            );
        }
        anyhow::ensure!(
            choice["delta"]["role"].is_null()
                || choice["delta"]["role"].as_str() == Some("assistant"),
            "streamed choice has a non-assistant role"
        );
        if let Some(calls) = delta.get("tool_calls").filter(|calls| !calls.is_null()) {
            for call in calls
                .as_array()
                .context("streamed tool calls are not an array")?
            {
                let index = call["index"]
                    .as_u64()
                    .context("streamed tool call has no integer index")?;
                self.calls.entry(index).or_default().append(call)?;
            }
        }
        if !choice["finish_reason"].is_null() {
            stable_string(
                &mut self.finish_reason,
                &choice["finish_reason"],
                "finish reason",
            )?;
        }
        self.saw_choice = true;
        Ok(())
    }

    pub(super) fn finish(self, round: OpenAiStreamRound) -> anyhow::Result<Value> {
        // A complete observed error is stronger evidence than a cut stream or
        // an unrelated malformed frame that preceded the server's rejection.
        if let Some(error) = self.provider_error.or(self.problem) {
            return Err(error);
        }
        anyhow::ensure!(round.done, "OpenAI stream ended before [DONE]");
        anyhow::ensure!(self.saw_choice, "OpenAI stream contained no choice");
        let finish = self
            .finish_reason
            .context("OpenAI stream has no finish reason")?;
        anyhow::ensure!(
            self.calls.is_empty() || finish == "tool_calls",
            "streamed tool batch did not finish with tool_calls"
        );
        anyhow::ensure!(
            finish != "tool_calls" || !self.calls.is_empty(),
            "stream finished tool_calls without any calls"
        );
        let mut ids = HashSet::new();
        let mut calls = Vec::new();
        for (position, (index, call)) in self.calls.into_iter().enumerate() {
            anyhow::ensure!(
                index == position as u64,
                "streamed tool call indices have a gap"
            );
            let id = call.id.context("streamed tool call has no ID")?;
            anyhow::ensure!(ids.insert(id.clone()), "streamed tool calls reuse an ID");
            let raw_arguments = Value::String(call.arguments);
            let (name, arguments) =
                crate::agentic::tools::validate_tool_call(Some(&call.name), &raw_arguments)
                    .map_err(anyhow::Error::msg)?;
            calls.push(json!({"id":id, "type":"function", "function":{
                "name":name,"arguments":arguments.to_string()}}));
        }
        let mut message = json!({"role":"assistant", "content":round.text});
        if !round.reasoning.is_empty() {
            message["reasoning_content"] = json!(round.reasoning);
        }
        if !calls.is_empty() {
            message["tool_calls"] = json!(calls);
        }
        let mut response =
            json!({"choices":[{"index":0,"message":message,"finish_reason":finish}]});
        if let Some(id) = self.id {
            response["id"] = json!(id);
        }
        if let Some(model) = self.model {
            response["model"] = json!(model);
        }
        if let Some(usage) = self.usage {
            response["usage"] = usage;
        }
        Ok(response)
    }
}

#[derive(Debug, Default)]
struct ToolFragments {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl ToolFragments {
    fn append(&mut self, call: &Value) -> anyhow::Result<()> {
        stable_string(&mut self.id, &call["id"], "tool-call ID")?;
        anyhow::ensure!(
            call["type"].is_null() || call["type"].as_str() == Some("function"),
            "streamed tool call is not a function"
        );
        if !call["function"].is_null() {
            anyhow::ensure!(
                call["function"].is_object(),
                "streamed tool function is not an object"
            );
            for (key, target) in [("name", &mut self.name), ("arguments", &mut self.arguments)] {
                if !call["function"][key].is_null() {
                    target.push_str(
                        call["function"][key]
                            .as_str()
                            .context("streamed function fragment is not a string")?,
                    );
                }
            }
        }
        Ok(())
    }
}

fn stable_string(target: &mut Option<String>, value: &Value, field: &str) -> anyhow::Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let value = value
        .as_str()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("stream has invalid {field}"))?;
    anyhow::ensure!(
        target.as_deref().is_none_or(|old| old == value),
        "stream changed {field}"
    );
    *target = Some(value.to_owned());
    Ok(())
}

pub(super) fn is_error(frame: &Value) -> bool {
    !frame["error"].is_null()
        || frame["type"].as_str() == Some("error")
        || frame["object"].as_str() == Some("error")
}

pub(super) fn provider_error(frame: &Value) -> anyhow::Error {
    let error = if frame["error"].is_null() {
        frame
    } else {
        &frame["error"]
    };
    let code = error["code"]
        .as_u64()
        .or_else(|| error["code"].as_str()?.parse().ok());
    let message = if let Some(code @ 400..=599) = code {
        format!("OpenAI stream returned {code}: {error}")
    } else {
        format!("OpenAI stream error: {error}")
    };
    ProviderError(message).into()
}

/// A complete server error envelope, distinct from malformed or cut framing.
#[derive(Debug)]
pub(super) struct ProviderError(String);

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProviderError {}
