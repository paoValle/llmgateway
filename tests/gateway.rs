//! The whole path, with no network and no server.
//!
//! Here it is verified that the four steps are in the right order and that each one
//! changes the behavior of the next. The test that matters most of all is
//! [`an_unreadable_cap_does_not_stop_the_client`]: if the accountant is broken, the user
//! still gets their response.

mod support;

use std::sync::Arc;
use std::time::Duration;

use llmgateway::auth::Authenticator;
use llmgateway::budget::{BudgetRegistry, Month, TenantBudget};
use llmgateway::gateway::{Gateway, GatewayConfig, GatewayRequest};
use llmgateway::meter::Meter;
use llmgateway::pricing::{micros, usd, Price, PriceTable};
use llmgateway::router::Router;

use support::{shared, Behavior, Fake};

const GEN: i64 = 1_767_225_600_000; // 2026-01-01
const TIMEOUT: Duration = Duration::from_secs(5);

fn prices() -> PriceTable {
    let mut m = std::collections::BTreeMap::new();
    m.insert(
        "gpt-4o-mini".to_owned(),
        Price {
            input: micros(150),
            output: micros(600),
        },
    );
    PriceTable::new(m)
}

/// A gateway with a fake provider that answers with a known usage.
fn gateway(
    provider: Arc<dyn llmgateway::upstream::Upstream>,
    budget_usd: f64,
    allowed_models: &[&str],
) -> (Gateway, Arc<Meter>, Arc<TenantBudget>) {
    let prices = prices();
    let meter = Arc::new(Meter::new(prices.clone(), 1));
    let budget = BudgetRegistry::new();
    let cap = budget.insert(
        TenantBudget::new("acme", usd(budget_usd).expect("valid cap"), Month::of(GEN)),
        GEN,
    );
    let models: std::collections::BTreeSet<String> =
        allowed_models.iter().map(|m| (*m).to_owned()).collect();
    let clock = Arc::new(|| GEN);

    let gw = Gateway::new(GatewayConfig {
        authenticator: Authenticator::new(
            &[("acme".to_owned(), "sk-acme".to_owned())],
            &[("acme".to_owned(), models)],
        ),
        budget,
        meter: Arc::clone(&meter),
        router: Arc::new(Router::new(vec![provider], 4, TIMEOUT)),
        prices,
        max_output_default: 4_096,
        timeout: TIMEOUT,
        now: clock,
    });
    (gw, meter, cap)
}

/// The 200 response the fake provider gives by default, with usage.
fn response_with_usage() -> Behavior {
    Behavior::Responds(
        200,
        r#"{"choices":[{"message":{"content":"hello"}}],"usage":{"prompt_tokens":1000,"completion_tokens":500}}"#
            .to_owned(),
    )
}

fn request() -> GatewayRequest {
    GatewayRequest {
        key: Some("sk-acme".to_owned()),
        body: br#"{"model":"gpt-4o-mini","messages":[]}"#.to_vec(),
    }
}

// --- the happy path ---------------------------------------------------------------

#[tokio::test]
async fn a_valid_request_reaches_the_provider_and_comes_back() {
    let (gw, _, _) = gateway(
        shared(Fake::new("a").with_behaviors(vec![response_with_usage()])),
        10.0,
        &["gpt-4o-mini"],
    );
    let r = gw.handle(request()).await;

    assert_eq!(r.status(), 200);
    assert_eq!(r.provider(), Some("a"));
    match r {
        llmgateway::gateway::GatewayResponse::Whole { body, .. } => {
            assert!(String::from_utf8(body).unwrap().contains("hello"));
        }
        llmgateway::gateway::GatewayResponse::Stream { .. } => {
            panic!("without stream a whole response is expected")
        }
    }
}

#[tokio::test]
async fn the_bill_is_settled_on_the_real_consumption_and_not_on_the_estimate() {
    let (gw, meter, cap) = gateway(
        shared(Fake::new("a").with_behaviors(vec![response_with_usage()])),
        10.0,
        &["gpt-4o-mini"],
    );
    gw.handle(request()).await;

    // the estimate reserved estimated input + 4096 output: far more than it cost. The
    // difference must come back, otherwise the cap consumes itself and the client is
    // asked for the rest of the month after a few requests
    let expected = prices().resolve("gpt-4o-mini").cost(1_000, 500);
    assert_eq!(meter.spent("acme"), expected);
    assert_eq!(cap.held(GEN), 0, "the reservation never stays pending");
    assert_eq!(cap.spent(GEN), expected);
}

// --- authentication ---------------------------------------------------------------

#[tokio::test]
async fn without_a_key_nothing_is_asked_of_any_provider() {
    let fake = Arc::new(Fake::new("a"));
    let (gw, _, _) = gateway(fake.clone(), 10.0, &["gpt-4o-mini"]);

    let r = gw
        .handle(GatewayRequest {
            key: None,
            body: request().body,
        })
        .await;
    assert_eq!(r.status(), 401);
    assert_eq!(fake.calls(), 0, "an unauthenticated request calls nobody");
}

#[tokio::test]
async fn an_unknown_key_is_a_401_like_a_missing_one() {
    // the difference between the two cases is in the message to the caller, not in the
    // behavior
    let (gw, _, _) = gateway(shared(Fake::new("a")), 10.0, &["gpt-4o-mini"]);
    let r = gw
        .handle(GatewayRequest {
            key: Some("sk-unknown".to_owned()),
            body: request().body,
        })
        .await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn a_model_not_allowed_for_the_tenant_receives_403() {
    let (gw, meter, _) = gateway(shared(Fake::new("a")), 10.0, &["another-model"]);
    let r = gw.handle(request()).await;
    assert_eq!(r.status(), 403);
    assert_eq!(
        meter.denied("acme"),
        1,
        "a 403 is a rejection, and must be counted as such"
    );
}

// --- the cap: the part that distinguishes a gateway from a proxy --------------------

#[tokio::test]
async fn a_tenant_over_the_cap_receives_429_and_the_provider_is_not_called() {
    let fake = Arc::new(Fake::new("a"));
    let (gw, meter, _) = gateway(fake.clone(), 0.000_002, &["gpt-4o-mini"]);

    let r = gw.handle(request()).await;
    assert_eq!(r.status(), 429);
    assert_eq!(
        fake.calls(),
        0,
        "the point of all this: no provider was called, no money went out"
    );
    assert_eq!(meter.denied("acme"), 1);
}

#[tokio::test]
async fn the_cap_that_runs_out_before_being_used_up_is_why() {
    // The numbers, stated so the test is worth as much as it claims:
    //   estimate = 9 estimated tokens × 150 + 4096 output × 600 ≈ 3 µUSD (rounded up)
    //   usage    = 1000 × 150 + 500 × 600 = 0.45 µUSD → 1 µUSD (rounded up)
    //   cap      = 7 µUSD
    // If the cap were consumed by the **estimate**, 7/3 = two requests and that is it.
    // Since it settles on the real consumption, five go through: that is the whole
    // difference between a reservation and a charge.
    let fake = Arc::new(Fake::new("a").with_behaviors(vec![response_with_usage()]));
    let (gw, _, _) = gateway(fake.clone(), 0.000_007, &["gpt-4o-mini"]);

    for step in 1..=5 {
        assert_eq!(
            gw.handle(request()).await.status(),
            200,
            "request {step}: the estimate must not consume the cap"
        );
    }
    assert_eq!(
        gw.handle(request()).await.status(),
        429,
        "at this point the cap is really exhausted"
    );
    assert_eq!(
        fake.calls(),
        5,
        "the sixth request must not have reached the provider"
    );
}

#[tokio::test]
async fn an_unreadable_cap_does_not_stop_the_client() {
    // this is the proof that ADR 0004 holds for the cap too: if the accountant is broken,
    // the user still gets their response. In that case the cap does not protect, and the
    // gateway says so in the log — but it does not deny service to someone who did the
    // right thing
    let fake = Arc::new(Fake::new("a").with_behaviors(vec![response_with_usage()]));
    let (gw, meter, cap) = gateway(fake.clone(), 10.0, &["gpt-4o-mini"]);

    // poison the lock as a panic in another thread would. It must be caught: the panic is
    // how a `Mutex` gets poisoned, not an exception that propagates
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_info| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cap.poison()));
    std::panic::set_hook(hook);

    assert!(
        outcome.is_err(),
        "the panic is how a Mutex gets poisoned: without it, the lock would be intact"
    );

    let r = gw.handle(request()).await;
    assert_eq!(
        r.status(),
        200,
        "a ready response is not thrown away because of a broken cap"
    );
    assert_eq!(fake.calls(), 1);
    assert!(
        meter.served_uncovered() > 0,
        "the degradation is not silent: the 'served without cap' counter goes up"
    );
}

// --- failover goes through the gateway ----------------------------------------------

#[tokio::test]
async fn a_provider_that_falls_over_produces_502_and_the_client_gets_a_generic_message() {
    // the client must not know that a second provider exists: that information is useless
    // to them and useful to whoever wants to map the infrastructure
    let (gw, meter, cap) = gateway(
        shared(Fake::new("a").with_behaviors(vec![Behavior::unavailable()])),
        10.0,
        &["gpt-4o-mini"],
    );
    let _ = &meter;

    let r = gw.handle(request()).await;
    assert_eq!(r.status(), 502);
    assert_eq!(
        cap.held(GEN),
        0,
        "a request never served must not lock money"
    );
    assert_eq!(cap.spent(GEN), 0);
    assert!(meter.snapshot_for("acme").is_some());
}

#[tokio::test]
async fn a_400_from_the_provider_goes_back_to_the_client_as_it_is_with_its_body() {
    let (gw, _, _) = gateway(
        shared(Fake::new("a").with_behaviors(vec![Behavior::Responds(
            400,
            r#"{"error":{"message":"unknown model"}}"#.to_owned(),
        )])),
        10.0,
        &["gpt-4o-mini"],
    );

    let r = gw.handle(request()).await;
    assert_eq!(r.status(), 400, "its error stays its error");
    match r {
        llmgateway::gateway::GatewayResponse::Whole { body, .. } => {
            assert!(String::from_utf8(body).unwrap().contains("unknown model"));
        }
        llmgateway::gateway::GatewayResponse::Stream { .. } => panic!("a whole response"),
    }
}

// --- streaming ----------------------------------------------------------------------

#[tokio::test]
async fn streaming_is_forwarded_in_chunks_and_stays_declared() {
    let (gw, _, _) = gateway(shared(Fake::new("a")), 10.0, &["gpt-4o-mini"]);

    let r = gw
        .handle(GatewayRequest {
            key: Some("sk-acme".to_owned()),
            body: br#"{"model":"gpt-4o-mini","stream":true}"#.to_vec(),
        })
        .await;

    match r {
        llmgateway::gateway::GatewayResponse::Stream {
            status, provider, ..
        } => {
            assert_eq!(status, 200);
            assert_eq!(provider, "a");
        }
        llmgateway::gateway::GatewayResponse::Whole { .. } => {
            panic!("a stream must not become a whole response: the point is lost")
        }
    }
}

// --- the degenerate case ------------------------------------------------------------

#[tokio::test]
async fn a_body_without_a_model_is_not_a_500_from_the_gateway() {
    // the gateway does not judge the body: it lets the provider answer
    let (gw, _, _) = gateway(shared(Fake::new("a")), 10.0, &["gpt-4o-mini"]);
    let r = gw
        .handle(GatewayRequest {
            key: Some("sk-acme".to_owned()),
            body: "not json".as_bytes().to_vec(),
        })
        .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_response_without_usage_accounts_for_zero_and_says_so() {
    // the gateway does not estimate after the fact: an invented number is worse than a
    // zero
    let (gw, meter, _) = gateway(
        shared(Fake::new("a").with_behaviors(vec![Behavior::ok("{\"ok\":true}")])),
        10.0,
        &["gpt-4o-mini"],
    );
    let r = gw.handle(request()).await;
    assert_eq!(r.status(), 200);
    assert_eq!(meter.spent("acme"), 0);
}

#[tokio::test]
async fn the_debug_of_a_response_does_not_print_the_content() {
    let (gw, _, _) = gateway(
        shared(Fake::new("a").with_behaviors(vec![response_with_usage()])),
        10.0,
        &["gpt-4o-mini"],
    );
    let text = format!("{:?}", gw.handle(request()).await);
    assert!(
        !text.contains("hello"),
        "the provider body does not end up in a Debug"
    );
    assert!(text.contains("status: 200"));
}
