//! Failover, verified without touching the network.
//!
//! The three classes of ADR 0003 each have at least one test. If one of the three is not
//! covered, the ADR's table is only documentation.

mod support;

use std::time::Duration;

use llmgateway::router::{RouteError, Router, DEFAULT_MAX_ATTEMPTS};
use llmgateway::upstream::{ResponseBody, TransportKind, UpstreamRequest};

use support::{shared, Behavior, Fake};

const TIMEOUT: Duration = Duration::from_secs(5);

fn request(model: &str) -> llmgateway::upstream::UpstreamRequest {
    UpstreamRequest::new(br#"{"messages":[]}"#.to_vec(), model, false)
}

fn router(
    providers: Vec<std::sync::Arc<dyn llmgateway::upstream::Upstream>>,
    max: usize,
) -> Router {
    Router::new(providers, max, TIMEOUT)
}

#[tokio::test]
async fn the_first_provider_that_answers_wins_and_the_others_are_not_started() {
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::ok("{\"from\":\"a\"}")]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"from\":\"b\"}")]));
    let r = router(vec![a, b], 4)
        .route(request("gpt-4o-mini"))
        .await
        .expect("served");

    assert_eq!(r.provider, "a");
    assert_eq!(r.attempts, 1);
    match r.response.body {
        ResponseBody::Buffered(b) => assert_eq!(String::from_utf8(b).unwrap(), "{\"from\":\"a\"}"),
        ResponseBody::Stream(_) => {
            panic!("a response not requested as streaming must not be one")
        }
    }
}

// --- class 1: provider error, it is retried ------------------------------------------

#[tokio::test]
async fn a_503_moves_to_the_next_provider_and_the_client_does_not_notice() {
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::unavailable()]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    let r = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect("the second one must answer");

    assert_eq!(r.provider, "b");
    assert_eq!(r.attempts, 2, "the first attempt was counted");
}

#[tokio::test]
async fn a_429_moves_to_the_next_provider_even_though_it_is_a_4xx() {
    let a = shared(
        Fake::new("a").with_behaviors(vec![Behavior::Responds(429, "{\"e\":\"rate\"}".into())]),
    );
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    let r = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect("the second one must answer");
    assert_eq!(r.provider, "b");
}

#[tokio::test]
async fn a_refused_connection_moves_on_to_the_next_one() {
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::NotSent(TransportKind::Connect)]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    let r = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect("the second one must answer");
    assert_eq!(r.provider, "b");
}

// --- class 2: client error, no failover ----------------------------------------------

#[tokio::test]
async fn a_400_is_not_retried_on_any_provider() {
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::Responds(
        400,
        "{\"error\":\"bad request\"}".into(),
    )]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));
    let error = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect_err("a 400 goes to the client");

    match error {
        RouteError::ClientFault {
            provider,
            status,
            body,
        } => {
            assert_eq!(provider, "a");
            assert_eq!(status, 400);
            assert!(
                String::from_utf8(body).unwrap().contains("bad request"),
                "the body must be forwarded"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn a_400_on_all_providers_produces_a_response_and_not_an_exhaustion() {
    // the client got it wrong: telling them "we tried three providers" does not help,
    // and spending three attempts on an error of theirs is wasted time
    let a =
        shared(Fake::new("a").with_behaviors(vec![Behavior::Responds(400, "{\"e\":1}".into())]));
    let b =
        shared(Fake::new("b").with_behaviors(vec![Behavior::Responds(400, "{\"e\":2}".into())]));

    let error = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect_err("400 everywhere");
    assert!(matches!(error, RouteError::ClientFault { .. }), "{error:?}");
}

// --- class 3: unknown state, no failover ---------------------------------------------

#[tokio::test]
async fn an_unclassified_status_stops_failover() {
    // a 301 from a chat completions provider is something the gateway does not know how
    // to interpret. Amplifying it across three providers would be worse than propagating
    // it: the error stays, but with its cause and not as a "provider not responding"
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::Responds(301, "moved".into())]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    let error = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect_err("a 301 is not retried");
    match error {
        RouteError::UnknownStatus {
            provider, status, ..
        } => {
            assert_eq!(provider, "a");
            assert_eq!(status, 301);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// --- the rule that matters most: no double charge ------------------------------------

#[tokio::test]
async fn a_request_already_sent_is_not_retried_even_if_the_provider_looks_healthy() {
    // the provider went away after receiving the request: it may have executed it.
    // Retrying on another provider can execute it twice.
    let a = shared(
        Fake::new("a").with_behaviors(vec![Behavior::SentWithoutResponse(TransportKind::Reset)]),
    );
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    let error = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect_err("no failover");

    match error {
        RouteError::DeliveryUnknown { provider, error } => {
            assert_eq!(provider, "a");
            assert_eq!(error.kind, TransportKind::Reset);
            assert!(!error.delivery.safe_to_retry());
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// --- the attempt cap -----------------------------------------------------------------

#[tokio::test]
async fn the_attempt_cap_across_all_providers_stops_failover() {
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::unavailable()]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::unavailable()]));
    let c = shared(Fake::new("c").with_behaviors(vec![Behavior::unavailable()]));
    let d = shared(Fake::new("d").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    // with a cap of 2, the fourth provider must never be called
    let error = router(vec![a, b, c, d], 2)
        .route(request("m"))
        .await
        .expect_err("exhausted");

    match error {
        RouteError::Exhausted { attempts, .. } => assert_eq!(attempts, 2),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn a_cap_of_zero_does_not_mean_no_attempts() {
    // zero attempts would be a gateway that serves nobody: the default is 4
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));
    let r = router(vec![a], 0);
    assert_eq!(r.max_attempts(), DEFAULT_MAX_ATTEMPTS);
    assert!(r.route(request("m")).await.is_ok());
}

// --- routing by model ----------------------------------------------------------------

#[tokio::test]
async fn a_provider_that_does_not_serve_the_model_is_skipped_and_that_is_not_an_error() {
    // it is not a failure: it is a routing choice. Counting it as an error would raise an
    // alarm every time the model changes
    let only_a = shared(Fake::with_models("only-a", &["model-a"]));
    let only_b = shared(Fake::with_models("only-b", &["model-b"]));

    let r = router(vec![only_a, only_b], 4)
        .route(request("model-b"))
        .await
        .expect("model-b");

    assert_eq!(r.provider, "only-b");
    assert_eq!(r.attempts, 1, "only-b received one request");
}

#[tokio::test]
async fn a_model_nobody_serves_says_what_is_served_and_what_is_not() {
    let only_a = shared(Fake::with_models("only-a", &["model-a"]));

    let error = router(vec![only_a], 4)
        .route(request("model-z"))
        .await
        .expect_err("nobody serves it");

    match error {
        RouteError::NoProviderForModel { model, available } => {
            assert_eq!(model, "model-z");
            assert_eq!(available, vec!["model-a".to_owned()]);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn a_provider_that_serves_all_models_has_no_list_to_maintain() {
    let generic = shared(Fake::new("generic"));
    assert!(router(vec![generic], 4)
        .route(request("anything"))
        .await
        .is_ok());
}

#[tokio::test]
async fn without_providers_the_router_says_so_and_tries_nothing() {
    let error = router(vec![], 4)
        .route(request("m"))
        .await
        .expect_err("no provider");
    assert!(matches!(error, RouteError::NoProviders));
}

// --- streaming -----------------------------------------------------------------------

#[tokio::test]
async fn streaming_arrives_in_chunks_and_not_as_a_single_body() {
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::Responds(
        200,
        "{\"chunk\":1}\n{\"chunk\":2}\n{\"chunk\":3}".to_owned(),
    )]));

    let r = router(vec![a], 4)
        .route(UpstreamRequest::new(b"{}".to_vec(), "m", true))
        .await
        .expect("streaming served");

    assert!(
        r.response.body.is_stream(),
        "a stream must not become an in-memory body"
    );
}

// --- provider order ------------------------------------------------------------------

#[tokio::test]
async fn providers_are_tried_in_the_order_they_are_given() {
    let a = shared(Fake::new("a").with_behaviors(vec![Behavior::unavailable()]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::unavailable()]));
    let c = shared(Fake::new("c").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    let r = router(vec![a, b, c], 5)
        .route(request("m"))
        .await
        .expect("served by c");
    assert_eq!(r.provider, "c");
    assert_eq!(r.attempts, 3);
}

#[tokio::test]
async fn the_call_count_tells_where_the_attempts_went() {
    let a = std::sync::Arc::new(Fake::new("a").with_behaviors(vec![Behavior::unavailable()]));
    let b = std::sync::Arc::new(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));
    let r = router(vec![a.clone(), b.clone()], 4);

    let response = r.route(request("m")).await.expect("served by b");
    assert_eq!(response.provider, "b");

    // the first provider was called once and failed: counting it is what distinguishes
    // "it tried two providers" from "it retried the same one twice"
    assert_eq!(a.calls(), 1);
    assert_eq!(b.calls(), 1);
    assert_eq!(a.requested_models(), vec!["m".to_owned()]);
    assert_eq!(b.requested_models(), vec!["m".to_owned()]);
}

#[tokio::test]
async fn a_skipped_provider_is_not_counted_as_called() {
    let a = std::sync::Arc::new(Fake::with_models("only-a", &["other"]));
    let b = std::sync::Arc::new(Fake::new("b"));
    let r = router(vec![a.clone(), b.clone()], 4);

    r.route(request("m")).await.expect("served by b");
    assert_eq!(a.calls(), 0, "skipping a provider is not calling it");
}

#[tokio::test]
async fn a_4xx_the_gateway_does_not_interpret_stops_failover_like_any_other_4xx() {
    // 451 is a 4xx: it is not an "unknown status", it is the client's like a 400.
    // The router stops in both cases, which is the point
    let a =
        shared(Fake::new("a").with_behaviors(vec![Behavior::Responds(451, "{\"e\":1}".into())]));
    let b = shared(Fake::new("b").with_behaviors(vec![Behavior::ok("{\"ok\":true}")]));

    let error = router(vec![a, b], 4)
        .route(request("m"))
        .await
        .expect_err("a 451 is not retried");
    assert!(
        matches!(error, RouteError::ClientFault { status: 451, .. }),
        "{error:?}"
    );
}
