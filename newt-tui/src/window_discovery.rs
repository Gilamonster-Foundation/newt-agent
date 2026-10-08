//! Session-local, nonblocking retry for metadata that missed startup's short probe.
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;
use tokio::sync::oneshot;

// A busy router can forward /props in ~15 seconds; retry off the UI thread.
const WINDOW_WAIT: Duration = Duration::from_secs(30);
type Answer = Result<u32, &'static str>;
type Route = (String, String);

#[derive(Default)]
pub(crate) struct WindowDiscovery {
    attempted: HashSet<Route>,
    pending: HashMap<Route, (tokio::task::JoinHandle<()>, oneshot::Receiver<Answer>)>,
}

async fn bounded_window(probe: impl Future<Output = Option<u32>>) -> Answer {
    match tokio::time::timeout(WINDOW_WAIT, probe).await {
        Ok(Some(window)) if window > 0 => Ok(window),
        Ok(_) => Err("server metadata returned no usable context window"),
        Err(_) => Err("context-window metadata probe timed out"),
    }
}

impl WindowDiscovery {
    /// Apply a completed result at a turn boundary, before budget resolution.
    /// Only the matching active route receives it; failed probes are reported
    /// once and leave the gauge's unknown-window marker intact.
    pub(crate) fn update(
        &mut self,
        choice: &mut crate::BackendChoice,
        window: &mut Option<u32>,
    ) -> Option<String> {
        if window.is_some() || choice.kind != newt_core::BackendKind::Openai {
            return None;
        }
        match self.poll(&choice.url, choice.active_model.as_deref().unwrap_or_default(), choice.api_key.as_deref())? {
            Ok(found) => {
                *window = Some(found);
                choice.context_window = Some(found);
                Some(format!("context window discovered: {found} tokens"))
            }
            Err(reason) => Some(format!("context window remains unknown (?): {reason}; using estimates until discovery succeeds on a later connection")),
        }
    }
    /// Starts once per endpoint/model and consumes each result once. No wait on
    /// the input thread; a route switch cannot adopt another model's answer.
    fn poll(&mut self, endpoint: &str, model: &str, api_key: Option<&str>) -> Option<Answer> {
        if model.is_empty() {
            return None;
        }
        let route = (endpoint.to_owned(), model.to_owned());
        if let Some((_, receiver)) = self.pending.get_mut(&route) {
            let answer = match receiver.try_recv() {
                Ok(answer) => answer,
                Err(oneshot::error::TryRecvError::Empty) => return None,
                Err(oneshot::error::TryRecvError::Closed) => {
                    Err("context-window metadata probe stopped")
                }
            };
            self.pending.remove(&route);
            return Some(answer);
        }
        if self.attempted.insert(route.clone()) {
            let (sender, receiver) = oneshot::channel();
            let (endpoint, model) = route.clone();
            let api_key = api_key.map(str::to_owned);
            let task = tokio::spawn(async move {
                let answer = bounded_window(async {
                    let client = reqwest::Client::new();
                    newt_core::backend_probe::api_for(newt_core::BackendKind::Openai)
                        .context_window(&client, &endpoint, &model, api_key.as_deref())
                        .await
                })
                .await;
                let _ = sender.send(answer);
            });
            self.pending.insert(route, (task, receiver));
        }
        None
    }
}

impl Drop for WindowDiscovery {
    fn drop(&mut self) {
        for (task, _) in self.pending.values() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2248: the router can take 15 seconds even for an already-loaded model.
    /// The injected delayed HTTP result uses Tokio's clock, never wall time.
    #[tokio::test]
    async fn slow_metadata_outlives_the_old_startup_probe_bound() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .and(query_param("model", "example/model:Q8_0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings": {"n_ctx": 131072}
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Obtain the mocked HTTP result, then inject its delivery latency.
        // Only the delivery clock advances; the HTTP mock needs no real delay.
        let observed = newt_core::backend_probe::api_for(newt_core::BackendKind::Openai)
            .context_window(
                &reqwest::Client::new(),
                &server.uri(),
                "example/model:Q8_0",
                None,
            )
            .await;
        tokio::time::pause();
        let answer = bounded_window(async {
            tokio::time::sleep(Duration::from_secs(15)).await;
            observed
        })
        .await;
        assert_eq!(answer, Ok(131072));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_metadata_is_bounded() {
        assert!(bounded_window(std::future::pending()).await.is_err());
    }

    /// #2248: a late answer is route-scoped, updates the active window once,
    /// and cannot silently overwrite another model after a switch.
    #[tokio::test]
    async fn background_answer_updates_only_its_route_and_reports_failure_once() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .and(query_param("model", "first"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings": {"n_ctx": 131072}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .and(query_param("model", "second"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings": {"n_ctx": 0}, "model_path": "none"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut discovery = WindowDiscovery::default();
        let mut choice = crate::BackendChoice::synthesized(
            "test",
            server.uri(),
            newt_core::BackendKind::Openai,
            Some("first".into()),
        );
        let mut window = None;
        assert!(discovery.update(&mut choice, &mut window).is_none());
        let route = (server.uri(), "first".into());
        (&mut discovery.pending.get_mut(&route).unwrap().0)
            .await
            .unwrap();
        choice.active_model = Some("second".into());
        assert!(discovery.update(&mut choice, &mut window).is_none());
        assert_eq!(window, None);
        let route = (server.uri(), "second".into());
        (&mut discovery.pending.get_mut(&route).unwrap().0)
            .await
            .unwrap();
        assert!(discovery
            .update(&mut choice, &mut window)
            .unwrap()
            .contains("no usable context window"));
        assert!(discovery.update(&mut choice, &mut window).is_none());
        choice.active_model = Some("first".into());
        assert!(discovery
            .update(&mut choice, &mut window)
            .unwrap()
            .contains("131072"));
        assert_eq!(window, Some(131072));
        assert_eq!(choice.context_window, window);
        assert!(discovery.update(&mut choice, &mut window).is_none());
    }

    #[tokio::test]
    async fn zero_window_stays_unknown() {
        assert!(bounded_window(async { Some(0) }).await.is_err());
    }
}
