//! Capture every primary inference request as an attempt in the #2313 ledger —
//! at the send, from the exact bytes on the wire.
//!
//! Recording where round usage is merged cannot count attempts honestly: one
//! merged round can be a probe plus a stream reissue, and every send sits inside
//! a retry loop that resends identical bytes. Keying the attempt from the built
//! request's body is the only point that sees exactly one attempt per HTTP
//! request, and it sees smart-harness projected bytes as they are sent.

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The turn's interrupt flag: an attempt dropped unsettled while it is set
    /// was cancelled (see [`Attempt`]).
    pub(crate) cancel: Option<&'a AtomicBool>,
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

/// One sent attempt, settled exactly once.
///
/// Deliberately neither `Clone` nor `Copy`: a copy could record one key twice.
/// Every explicit ending — [`complete`], [`finish`], [`failed`] — settles it.
/// When the future holding it is dropped first, as the loop's `cancellable`
/// race does when the operator interrupts, nothing else can run, so `Drop`
/// records it `cancelled` with no usage: a dropped read reported nothing we can
/// trust. Dropped unsettled with the flag clear, it is an error path whose
/// send-time `failed` record stands, and nothing is added.
pub(crate) struct Attempt<'a> {
    scope: AttemptScope<'a>,
    key: AttemptKey,
    settled: AtomicBool,
}

impl Attempt<'_> {
    fn settle(&self, state: AttemptState, usage: Option<TokenUsage>) {
        self.settled.store(true, Ordering::Relaxed);
        self.scope.observe(&self.key, usage, state);
    }
}

impl std::fmt::Debug for Attempt<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempt")
            .field("key", &self.key)
            .field("settled", &self.settled)
            .finish_non_exhaustive()
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !*self.settled.get_mut() && super::is_cancelled(self.scope.cancel) {
            self.scope.observe(&self.key, None, AttemptState::Cancelled);
        }
    }
}

/// Send one inference request.
///
/// With a scope, the attempt is keyed by the request's exact wire bytes and
/// recorded `failed` with no usage the moment it is sent: an attempt is failed
/// until it is settled otherwise, or cancelled (see [`Attempt`]). A transport error, a non-2xx response,
/// or a failure path nothing has wired yet therefore still counts exactly once
/// and never over-claims success. A request with no in-memory body cannot be
/// keyed (every such attempt would share one id) and is refused unsent.
pub(crate) async fn send<'a>(
    scope: Option<AttemptScope<'a>>,
    role: &str,
    request: reqwest::RequestBuilder,
    failure: &str,
) -> anyhow::Result<(reqwest::Response, Option<Attempt<'a>>)> {
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
    let attempt = Attempt {
        scope,
        key,
        settled: AtomicBool::new(false),
    };
    // Held across the request, so an interrupt that drops the send cancels it.
    let response = client.execute(request).await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            attempt.settled.store(true, Ordering::Relaxed);
            return Err(dispatch_error(error));
        }
    };
    Ok((response, Some(attempt)))
}

/// Record a sent attempt as complete with the usage its response reported.
pub(crate) fn complete(attempt: Option<&Attempt<'_>>, usage: Option<TokenUsage>) {
    finish(attempt, AttemptState::Ok, usage);
}

/// Record how a sent attempt ended, attaching whatever usage the server
/// reported whatever the state (#2313). `ok` is a complete terminal response —
/// truncation included; `failed` is a transport error, non-2xx, cut stream,
/// error event, or failed body; `cancelled` is the operator's interrupt.
pub(crate) fn finish(
    attempt: Option<&Attempt<'_>>,
    state: AttemptState,
    usage: Option<TokenUsage>,
) {
    if let Some(attempt) = attempt {
        attempt.settle(state, usage);
    }
}

/// Settle a failed response, keeping any usage it still reported. The send
/// already recorded the attempt failed, so without reported usage nothing is
/// added.
pub(crate) fn failed(attempt: Option<&Attempt<'_>>, error: &anyhow::Error) {
    let Some(attempt) = attempt else { return };
    match super::observability::reported_usage(error) {
        Some(usage) => attempt.settle(AttemptState::Failed, Some(usage)),
        None => attempt.settled.store(true, Ordering::Relaxed),
    }
}

#[cfg(test)]
#[path = "attempt_capture_tests.rs"]
mod tests;
