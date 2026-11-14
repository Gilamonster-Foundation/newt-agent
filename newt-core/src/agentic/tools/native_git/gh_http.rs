//! Request-boundary contract. The red commit contains only test code; the
//! pass-through models the currently unrestricted gh HTTP transport.
use reqwest::{header::HeaderMap, redirect::Policy, Request, Response};

type Reply = (u16, HeaderMap, Vec<u8>);

async fn forward<F: std::future::Future<Output = Result<Response, String>>>(
    _approved: &[String],
    _refused: &mut Option<String>,
    request: Request,
    send: impl FnOnce(Request, Policy) -> F,
) -> Result<Reply, String> {
    let response = send(request, Policy::default()).await?;
    Ok((
        response.status().as_u16(),
        response.headers().clone(),
        response.bytes().await.map_err(|e| e.to_string())?.to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(host: &str) -> Request {
        Request::new(
            reqwest::Method::GET,
            format!("https://{host}/read").parse().unwrap(),
        )
    }

    #[tokio::test]
    async fn gh_http_only_approved_destinations_reach_upstream() {
        for (host, expected) in [("forge.example", 200), ("ungranted.example", 403)] {
            let mut refused = None;
            let sent = std::cell::Cell::new(0);
            let reply = forward(
                &["forge.example".into()],
                &mut refused,
                request(host),
                |_, _| {
                    sent.set(sent.get() + 1);
                    std::future::ready(Ok(http::Response::new("read body").into()))
                },
            )
            .await
            .unwrap();
            assert_eq!(
                reply.0, expected,
                "unapproved request crossed the HTTP boundary"
            );
            assert_eq!(sent.get(), usize::from(expected == 200));
            assert_eq!(refused.as_deref(), (expected == 403).then_some(host));
        }
    }

    #[tokio::test]
    async fn gh_http_upstream_redirects_are_disabled() {
        let mut refused = None;
        let reply = forward(
            &["forge.example".into()],
            &mut refused,
            request("forge.example"),
            |_, policy| {
                assert_eq!(
                    format!("{policy:?}"),
                    "Policy(None)",
                    "upstream must return redirects to the checked socket boundary"
                );
                std::future::ready(Ok(http::Response::builder()
                    .status(302)
                    .header("location", "https://ungranted.example/read")
                    .body("")
                    .unwrap()
                    .into()))
            },
        )
        .await
        .unwrap();
        assert_eq!(reply.0, 302);
        assert!(refused.is_none());
    }
}
