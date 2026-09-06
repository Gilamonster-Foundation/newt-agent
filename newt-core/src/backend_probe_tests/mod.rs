use super::*;
use crate::config::{BackendKind, OpenAiApi as OpenAiApiSurface};
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn openai_backend(model: Option<&str>, serving: Option<Serving>) -> BackendConfig {
    BackendConfig {
        name: "b".into(),
        endpoint: "http://h:8000".into(),
        model: model.map(str::to_string),
        kind: Some(BackendKind::Openai),
        serving,
        ..Default::default()
    }
}

// Families beside this file. Both attributes are required: rustc needs only
// the `#[path]`, but the ratchets' shared scanner resolves a child ONLY when
// a `#[cfg(test)]` immediately precedes the `mod` (#2149).
#[cfg(test)]
#[path = "adopt_warm.rs"]
mod adopt_warm;
#[cfg(test)]
#[path = "anthropic.rs"]
mod anthropic;
#[cfg(test)]
#[path = "detect_endpoint.rs"]
mod detect_endpoint;
#[cfg(test)]
#[path = "engine_fingerprint.rs"]
mod engine_fingerprint;
#[cfg(test)]
#[path = "generation_probe.rs"]
mod generation_probe;
#[cfg(test)]
#[path = "openai_api_surface.rs"]
mod openai_api_surface;
#[cfg(test)]
#[path = "serving_precedence.rs"]
mod serving_precedence;
#[cfg(test)]
#[path = "warm_models.rs"]
mod warm_models;
