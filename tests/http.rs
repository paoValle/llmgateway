//! The transport adapters, tested against a real HTTP provider.
//!
//! These tests exist because the two files they cover are the only ones that can be wrong in
//! a way unit tests with a fake `Upstream` cannot see: a header not forwarded, a body that was
//! re-serialized, a timeout that was classified as "safe to retry" when it was not.
//!
//! The provider is a real HTTP server on an ephemeral port, so the whole chain runs: client →
//! gateway → `reqwest` → provider.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use llmgateway::auth::Authenticator;
use llmgateway::budget::{BudgetRegistry, Month, TenantBudget};
use llmgateway::gateway::{Gateway, GatewayConfig};
use llmgateway::http_upstream::HttpUpstream;
use llmgateway::meter::Meter;
use llmgateway::pricing::{micros, Price, PriceTable};
use llmgateway::router::Router as ModelRouter;
use llmgateway::upstream::Upstream;
use llmgateway::{usd, MicroUsd};
use tokio::net::TcpListener;

const NOW: i64 = 1_767_225_600_000;
const TENANT: &str = "acme";
const KEY: &str = "sk-acme";
const MODEL: &str = "gpt-4o-mini";

/// What the fake provider does on its next call.
#[derive(Debug, Clone)]
enum Reply {
    /// A status and a body carrying usage.
    Ok(u16, String),
    /// Takes the request, then never answers: the case that must not be retried.
    Hang(Duration),
    /// A streamed body, one chunk per piece.
    Stream(Vec<String>),
}

struct Provider {
    replies: Mutex<Vec<Reply>>,
    calls: AtomicUsize,
    bodies: Mutex<Vec<Vec<u8>>>,
    auth: Mutex<Vec<String>>,
}

impl Provider {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().expect("bodies").clone()
    }

    fn auth(&self) -> Vec<String> {
        self.auth.lock().expect("auth").clone()
    }

    fn next(&self) -> Reply {
        let mut replies = self.replies.lock().expect("replies");
        if replies.len() == 1 {
            return replies[0].clone();
        }
        replies.remove(0)
    }
}

/// Starts a real HTTP provider and returns its base URL, without the `/chat/completions`.
async fn start_provider(replies: Vec<Reply>) -> (String, Arc<Provider>) {
    let provider = Arc::new(Provider {
        replies: Mutex::new(replies),
        calls: AtomicUsize::new(0),
        bodies: Mutex::new(Vec::new()),
        auth: Mutex::new(Vec::new()),
    });

    let router = Router::new()
        .route("/v1/chat/completions", post(serve))
        .with_state(Arc::clone(&provider));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}/v1"), provider)
}

async fn serve(State(provider): State<Arc<Provider>>, headers: HeaderMap, body: Bytes) -> Response {
    provider.calls.fetch_add(1, Ordering::SeqCst);
    provider.bodies.lock().expect("bodies").push(body.to_vec());
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        provider.auth.lock().expect("auth").push(value.to_owned());
    }

    match provider.next() {
        Reply::Ok(status, body) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        Reply::Hang(duration) => {
            tokio::time::sleep(duration).await;
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
        Reply::Stream(chunks) => {
            let stream =
                futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .expect("streaming response")
        }
    }
}

fn usage_body(prompt: u64, completion: u64) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"content":"ok"}}}}],"usage":{{"prompt_tokens":{prompt},"completion_tokens":{completion}}}}}"#
    )
}

fn prices() -> PriceTable {
    let mut map = BTreeMap::new();
    map.insert(
        MODEL.to_owned(),
        Price {
            input: micros(150),
            output: micros(600),
        },
    );
    PriceTable::new(map)
}

/// Builds a gateway whose providers are real HTTP servers, plus its HTTP surface.
async fn gateway_for(
    providers: Vec<(&str, String)>,
    cap: MicroUsd,
    timeout_ms: u64,
) -> (String, Arc<Meter>) {
    let prices = prices();
    let meter = Arc::new(Meter::new(prices.clone(), 1));
    let budgets = BudgetRegistry::new();
    budgets.insert(TenantBudget::new(TENANT, cap, Month::of(NOW)), NOW);
    let timeout = Duration::from_millis(timeout_ms);

    let upstreams: Vec<Arc<dyn Upstream>> = providers
        .into_iter()
        .map(|(name, base_url)| {
            Arc::new(HttpUpstream::new(name, base_url, "sk-provider", None)) as Arc<dyn Upstream>
        })
        .collect();

    let gateway = Arc::new(Gateway::new(GatewayConfig {
        authenticator: Authenticator::new(
            &[(TENANT.to_owned(), KEY.to_owned())],
            &[(TENANT.to_owned(), BTreeSet::new())],
        ),
        budget: budgets,
        meter: Arc::clone(&meter),
        router: Arc::new(ModelRouter::new(upstreams, 4, timeout)),
        prices,
        max_output_default: 4_096,
        timeout,
        now: Arc::new(|| NOW),
    }));

    let (address, _handle) = llmgateway::http::serve_ephemeral(gateway)
        .await
        .expect("serve the gateway");
    (format!("http://{address}"), meter)
}

fn request_body(max_tokens: u64) -> String {
    format!(
        r#"{{"model":"{MODEL}","max_tokens":{max_tokens},"messages":[{{"role":"user","content":"hello"}}]}}"#
    )
}

async fn send_request(url: &str, key: Option<&str>, body: &str) -> (u16, String, Option<String>) {
    let client = reqwest::Client::new();
    let mut builder = client
        .post(format!("{url}/v1/chat/completions"))
        .body(body.to_owned());
    if let Some(key) = key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = builder.send().await.expect("send");
    let status = response.status().as_u16();
    let provider = response
        .headers()
        .get("x-llmgateway-provider")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let text = response.text().await.expect("body");
    (status, text, provider)
}

#[tokio::test]
async fn a_request_travels_byte_for_byte_and_is_metered() {
    let (base, provider) = start_provider(vec![Reply::Ok(200, usage_body(1_000, 500))]).await;
    let (gateway, meter) =
        gateway_for(vec![("provider-a", base)], usd(1.0).expect("cap"), 5_000).await;

    let body = request_body(4_096);
    let (status, text, served_by) = send_request(&gateway, Some(KEY), &body).await;

    assert_eq!(status, 200);
    assert_eq!(served_by.as_deref(), Some("provider-a"));
    assert_eq!(text, usage_body(1_000, 500));
    // byte for byte: not a re-serialized JSON with reordered keys
    assert_eq!(provider.bodies(), vec![body.into_bytes()]);
    // the provider key is the gateway's, not the client's
    assert_eq!(provider.auth(), vec!["Bearer sk-provider".to_owned()]);
    // 1000 × 150 + 500 × 600 = 0.45 µUSD, rounded up
    assert_eq!(meter.spent(TENANT), 1);
}

#[tokio::test]
async fn an_unknown_key_is_401_and_no_provider_is_called() {
    let (base, provider) = start_provider(vec![Reply::Ok(200, usage_body(1, 1))]).await;
    let (gateway, _) = gateway_for(vec![("provider-a", base)], usd(1.0).expect("cap"), 5_000).await;

    let (anonymous, _, _) = send_request(&gateway, None, &request_body(10)).await;
    let (wrong, _, _) = send_request(&gateway, Some("sk-nope"), &request_body(10)).await;

    assert_eq!(anonymous, 401);
    assert_eq!(wrong, 401);
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn a_cap_is_enforced_before_the_provider_is_called() {
    let (base, provider) = start_provider(vec![Reply::Ok(200, usage_body(1_000, 500))]).await;
    let (gateway, meter) = gateway_for(vec![("provider-a", base)], micros(2), 5_000).await;

    let (status, _, _) = send_request(&gateway, Some(KEY), &request_body(4_096)).await;

    assert_eq!(status, 429);
    assert_eq!(
        provider.calls(),
        0,
        "the provider must not have been called"
    );
    assert_eq!(meter.spent(TENANT), 0);
}

#[tokio::test]
async fn a_503_fails_over_to_the_next_provider() {
    let (first, first_provider) =
        start_provider(vec![Reply::Ok(503, r#"{"error":"nope"}"#.to_owned())]).await;
    let (second, second_provider) = start_provider(vec![Reply::Ok(200, usage_body(10, 5))]).await;
    let (gateway, meter) = gateway_for(
        vec![("provider-a", first), ("provider-b", second)],
        usd(1.0).expect("cap"),
        5_000,
    )
    .await;

    let (status, _, served_by) = send_request(&gateway, Some(KEY), &request_body(10)).await;

    assert_eq!(status, 200);
    assert_eq!(served_by.as_deref(), Some("provider-b"));
    assert_eq!(first_provider.calls(), 1);
    assert_eq!(second_provider.calls(), 1);
    assert_eq!(meter.spent(TENANT), 1);
}

#[tokio::test]
async fn a_provider_that_hangs_after_receiving_is_not_retried() {
    // the request reached the provider: it may have been executed, so retrying could charge
    // twice. An error is better than a double charge (ADR 0003).
    let (slow, slow_provider) =
        start_provider(vec![Reply::Hang(Duration::from_millis(2_000))]).await;
    let (fast, fast_provider) = start_provider(vec![Reply::Ok(200, usage_body(10, 5))]).await;
    let (gateway, meter) = gateway_for(
        vec![("provider-a", slow), ("provider-b", fast)],
        usd(1.0).expect("cap"),
        150,
    )
    .await;

    let (status, _, _) = send_request(&gateway, Some(KEY), &request_body(10)).await;

    assert_eq!(status, 502);
    assert_eq!(slow_provider.calls(), 1);
    assert_eq!(
        fast_provider.calls(),
        0,
        "a request that may have been executed is not retried"
    );
    assert_eq!(
        meter.spent(TENANT),
        0,
        "nothing was served, so nothing is owed"
    );
}

#[tokio::test]
async fn a_streaming_response_is_forwarded_in_chunks() {
    let chunks = vec![
        "data: {\"token\":\"hel\"}\n\n".to_owned(),
        "data: {\"token\":\"lo\"}\n\n".to_owned(),
    ];
    let (base, _) = start_provider(vec![Reply::Stream(chunks.clone())]).await;
    let (gateway, _) = gateway_for(vec![("provider-a", base)], usd(1.0).expect("cap"), 5_000).await;

    let body = format!(r#"{{"model":"{MODEL}","stream":true,"messages":[]}}"#);
    let (status, text, served_by) = send_request(&gateway, Some(KEY), &body).await;

    assert_eq!(status, 200);
    assert_eq!(served_by.as_deref(), Some("provider-a"));
    assert_eq!(
        text,
        chunks.concat(),
        "the stream must arrive whole and in order"
    );
}

#[tokio::test]
async fn metrics_expose_exact_money_and_sampled_counters_by_name() {
    let (base, _) = start_provider(vec![Reply::Ok(200, usage_body(1_000, 500))]).await;
    let (gateway, _) = gateway_for(vec![("provider-a", base)], usd(1.0).expect("cap"), 5_000).await;
    send_request(&gateway, Some(KEY), &request_body(4_096)).await;

    let text = reqwest::get(format!("{gateway}/metrics"))
        .await
        .expect("metrics")
        .text()
        .await
        .expect("metrics body");

    assert!(
        text.contains(r#"llmgateway_spend_micro_usd{tenant="acme"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"llmgateway_sampled_requests_total{tenant="acme"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"llmgateway_served_without_cap_total{tenant="acme"} 0"#),
        "{text}"
    );
    assert!(
        text.contains(r#"llmgateway_budget_micro_usd{tenant="acme",kind="limit"}"#),
        "{text}"
    );
}

#[tokio::test]
async fn a_body_without_a_model_is_refused_by_the_gateway_and_reaches_no_provider() {
    // without a model there is no price and no routing, so there is nothing to forward to:
    // the 400 is the gateway's, and it is the only judgment it makes about a body
    let (base, provider) = start_provider(vec![Reply::Ok(200, usage_body(10, 5))]).await;
    let (gateway, _) = gateway_for(vec![("provider-a", base)], usd(1.0).expect("cap"), 5_000).await;

    let (status, text, _) = send_request(&gateway, Some(KEY), "not json at all").await;

    assert_eq!(status, 400);
    assert!(text.contains("missing model"), "{text}");
    assert_eq!(
        provider.calls(),
        0,
        "a request with no model cannot be routed anywhere"
    );
}

#[tokio::test]
async fn a_provider_error_reaches_the_client_with_its_own_status_and_body() {
    let (base, provider) = start_provider(vec![Reply::Ok(
        422,
        r#"{"error":{"message":"context length exceeded"}}"#.to_owned(),
    )])
    .await;
    let (gateway, _) = gateway_for(vec![("provider-a", base)], usd(1.0).expect("cap"), 5_000).await;

    let (status, text, _) = send_request(&gateway, Some(KEY), &request_body(4_096)).await;

    assert_eq!(status, 422, "the provider's error stays the provider's");
    assert!(text.contains("context length exceeded"));
    assert_eq!(
        provider.calls(),
        1,
        "a 422 is the client's fault: no failover, no repeat"
    );
}
