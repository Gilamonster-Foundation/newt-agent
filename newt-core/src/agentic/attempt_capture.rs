//! Capture every primary inference request as an attempt in the #2313 ledger —
//! at the send, from the exact bytes on the wire.
//!
//! Recording where round usage is merged cannot count attempts honestly: one
//! merged round can span several sends, and every send sits inside
//! a retry loop that resends identical bytes. Keying the attempt from the built
//! request's body is the only point that sees exactly one attempt per HTTP
//! request, and it sees smart-harness projected bytes as they are sent.

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::attempts::{AttemptKey, AttemptLedger, AttemptRecord, AttemptState};
use crate::TokenUsage;

/// One turn's handle on the attempt ledger.
#[derive(Clone, Copy)]
pub(crate) struct AttemptScope<'a> {
    pub(crate) ledger: &'a Mutex<AttemptLedger>,
    /// The turn's active-prompt address.
    pub(crate) turn: &'a str,
    pub(crate) model: &'a str,
    pub(crate) backend: &'a str,
}

impl AttemptScope<'_> {
    fn ledger(&self) -> MutexGuard<'_, AttemptLedger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Best effort, like every journal: accounting never undoes the request.
    fn observe(&self, key: &AttemptKey, usage: Option<TokenUsage>, state: AttemptState) {
        let recorded = AttemptRecord::new(key, self.model, self.backend, usage, state)
            .and_then(|record| self.ledger().observe(record));
        if let Err(error) = recorded {
            tracing::warn!(%error, "the attempt ledger could not record an inference attempt");
        }
    }
}

/// Send one inference request.
///
/// With a scope, the attempt is keyed by the request's exact wire bytes and
/// recorded `failed` with no usage the moment it is sent: an attempt is failed
/// until [`complete`] proves otherwise. A transport error, a non-2xx response,
/// or a failure path nothing has wired yet therefore still counts exactly once
/// and never over-claims success. A request with no in-memory body cannot be
/// keyed (every such attempt would share one id) and is refused unsent.
pub(crate) async fn send(
    scope: Option<AttemptScope<'_>>,
    role: &str,
    request: reqwest::RequestBuilder,
    failure: &str,
) -> anyhow::Result<(reqwest::Response, Option<AttemptKey>)> {
    let dispatch_error = |error| {
        anyhow::Error::new(super::observability::DispatchError::from_reqwest(
            failure, error,
        ))
    };
    let Some(scope) = scope else {
        return Ok((request.send().await.map_err(dispatch_error)?, None));
    };
    let (client, request) = request.build_split();
    let request = request.map_err(dispatch_error)?;
    let Some(bytes) = request.body().and_then(reqwest::Body::as_bytes) else {
        anyhow::bail!(
            "an inference request without an in-memory body cannot be recorded as an attempt"
        );
    };
    let key = scope.ledger().dispatch(scope.turn, role, bytes);
    scope.observe(&key, None, AttemptState::Failed);
    let response = client.execute(request).await.map_err(dispatch_error)?;
    Ok((response, Some(key)))
}

/// Record a sent attempt as complete with the usage its response reported.
pub(crate) fn complete(
    scope: Option<AttemptScope<'_>>,
    key: Option<&AttemptKey>,
    usage: Option<TokenUsage>,
) {
    finish(scope, key, AttemptState::Ok, usage);
}

/// Record how a sent attempt ended, attaching whatever usage the server
/// reported whatever the state (#2313). `ok` is a complete terminal response —
/// truncation included; `failed` is a transport error, non-2xx, cut stream,
/// error event, or failed body, and (until cancellation is modelled) an
/// interrupt.
pub(crate) fn finish(
    scope: Option<AttemptScope<'_>>,
    key: Option<&AttemptKey>,
    state: AttemptState,
    usage: Option<TokenUsage>,
) {
    if let (Some(scope), Some(key)) = (scope, key) {
        scope.observe(key, usage, state);
    }
}

/// Keep the usage a failed response still reported. The send already recorded
/// the attempt failed, so without reported usage there is nothing to add.
pub(crate) fn failed(
    scope: Option<AttemptScope<'_>>,
    key: Option<&AttemptKey>,
    error: &anyhow::Error,
) {
    if let Some(usage) = super::observability::reported_usage(error) {
        finish(scope, key, AttemptState::Failed, Some(usage));
    }
}

#[cfg(test)]
#[path = "attempt_capture_tests.rs"]
mod tests;
