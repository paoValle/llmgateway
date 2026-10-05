//! The gateway: lining up authentication, cap, failover and accounting.
//!
//! `Gateway::handle` is a **pure function** that goes from a request to a response.
//! There is no HTTP `Request` inside and no HTTP `Response` outside: the transport
//! adapter lives in [`crate::http`] and is a few lines long.
//!
//! The reason is the same as in `agentloop`: a cycle that calls the outside world is
//! testable only if the outside world is an interface. Here the test with a fake provider
//! and a hand-built `BudgetRegistry` verifies **the whole** path — authentication, cap,
//! failover, accounting — in microseconds, with no server.
//!
//! The order of the operations is not random, and each of the four steps is there for a
//! reason:
//!
//! 1. **authenticate**: who it is, and whether they may use this model.
//! 2. **read the body and estimate**: what it would cost *before* calling anyone.
//! 3. **reserve the cap**: if it does not fit, answer `429` and **no provider is
//!    called**. It is the difference between a cap and a report.
//! 4. **forward and settle**: with the real bill, and the difference is released.

use std::sync::Arc;
use std::time::Duration;

use crate::auth::{AuthError, Authenticator};
use crate::budget::{BudgetRegistry, ReserveError};
use crate::meter::{Meter, Outcome};
use crate::pricing::{MicroUsd, PriceTable, Usage};
use crate::request::{inspect, RequestShape};
use crate::router::{RouteError, Routed, Router};
use crate::upstream::{ResponseBody, UpstreamRequest};

/// A request that arrives at the gateway. It is not an HTTP `Request`: it is what the
/// gateway **uses**, and nothing more.
#[derive(Debug, Clone)]
pub struct GatewayRequest {
    /// The tenant key, if present. `None` is an unauthenticated request.
    pub key: Option<String>,
    /// The request body, byte for byte.
    pub body: Vec<u8>,
}

/// A gateway response. Like the request, it is not an HTTP `Response`.
pub enum GatewayResponse {
    /// In-memory response, ready to be served.
    Whole {
        /// The status.
        status: u16,
        /// The body, **exactly as it arrived** from the provider.
        body: Vec<u8>,
        /// Who answered, if anyone answered.
        provider: Option<String>,
    },
    /// Streaming response: the gateway did not accumulate it and must not.
    Stream {
        /// The status.
        status: u16,
        /// The chunks, in the order they arrive.
        chunks: crate::upstream::ByteStream,
        /// Who answered.
        provider: String,
    },
}

/// `Debug` by hand for the same reason as [`crate::upstream::ResponseBody`]: the content
/// of a stream is not formattable, and printing it into a log would end up with the
/// provider's response inside.
impl std::fmt::Debug for GatewayResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Whole {
                status,
                body,
                provider,
            } => f
                .debug_struct("Whole")
                .field("status", status)
                .field("bytes", &body.len())
                .field("provider", provider)
                .finish(),
            Self::Stream {
                status, provider, ..
            } => f
                .debug_struct("Stream")
                .field("status", status)
                .field("provider", provider)
                .finish(),
        }
    }
}

impl GatewayResponse {
    /// The status, in both cases.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Whole { status, .. } | Self::Stream { status, .. } => *status,
        }
    }

    /// Who answered, if anyone answered.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Whole { provider, .. } => provider.as_deref(),
            Self::Stream { provider, .. } => Some(provider),
        }
    }
}

/// How the gateway is built.
pub struct GatewayConfig {
    /// Who may use the gateway.
    pub authenticator: Authenticator,
    /// The per-tenant caps.
    pub budget: BudgetRegistry,
    /// The counter.
    pub meter: Arc<Meter>,
    /// The router.
    pub router: Arc<Router>,
    /// The prices per model, to estimate before the call.
    pub prices: PriceTable,
    /// Maximum output assumed when the client does not declare it.
    pub max_output_default: u64,
    /// The timeout of one upstream call.
    pub timeout: Duration,
    /// A function returning "now", in Unix milliseconds.
    ///
    /// It is a parameter and not a hidden `SystemTime::now()`: the tests must be able to
    /// change month without sleeping thirty days.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("timeout", &self.timeout)
            .field("max_output_default", &self.max_output_default)
            .finish_non_exhaustive()
    }
}

/// The gateway.
#[derive(Clone, Debug)]
pub struct Gateway {
    config: Arc<GatewayConfig>,
}

impl Gateway {
    /// Builds the gateway. The configuration is cheap to clone, but it is shared.
    #[must_use]
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// The meter behind this gateway.
    ///
    /// Exposed so an HTTP surface can answer `/metrics` from the same counters the gateway
    /// writes to: a second handle to the same counter is a second chance to diverge.
    #[must_use]
    pub fn meter(&self) -> &Arc<Meter> {
        &self.config.meter
    }

    /// The per-tenant caps behind this gateway.
    #[must_use]
    pub fn budgets(&self) -> &BudgetRegistry {
        &self.config.budget
    }

    /// Handles a request.
    ///
    /// It never returns `Err`: every failure is a response, because from the client's
    /// point of view **everything** is a response. An `Err` here would mean the gateway
    /// crashed, and in that case the only right response is to make it noticeable.
    pub async fn handle(&self, request: GatewayRequest) -> GatewayResponse {
        let now = (self.config.now)();

        // --- 1. authentication ---
        let tenant = match self.config.authenticator.identify(request.key.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                // an invalid key is not an error to log as a failure: it arrives on every
                // request if someone got the configuration wrong
                tracing::info!(error = %e, "unauthenticated request");
                return Self::reject(e);
            }
        };

        // --- 2. reading the body and estimating ---
        let shape = inspect(&request.body, self.config.max_output_default);
        let Some(model) = shape.model.clone() else {
            // no model: no price, no routing, and nothing to forward *to*. This is the only
            // judgment the gateway makes about the body, and it makes it because without a
            // model there is no provider that could answer instead
            tracing::info!(tenant = %tenant.id, "request with no readable model");
            return Self::from_gateway(
                400,
                b"{\"error\":{\"message\":\"missing model\"}}".to_vec(),
                None,
            );
        };

        if !self.config.authenticator.can_use(&tenant.id, &model) {
            tracing::info!(tenant = %tenant.id, model = %model, "model not allowed for the tenant");
            self.config.meter.record_budget_denied(&tenant.id);
            return Self::from_gateway(
                403,
                br#"{"error":{"message":"model not allowed for this tenant","type":"permission_error"}}"#.to_vec(),
                None,
            );
        }

        let cap = self.config.budget.get(&tenant.id);
        let Some(cap) = cap else {
            // an authenticated tenant with no cap is an inconsistent configuration:
            // continuing would mean spending without control
            tracing::error!(tenant = %tenant.id, "tenant with no registered cap");
            return Self::from_gateway(
                500,
                b"{\"error\":{\"message\":\"tenant has no budget\"}}".to_vec(),
                None,
            );
        };

        let (price, estimated) = self.config.meter.price_for(&model);
        let estimate = price.cost(
            shape.estimated_input_tokens,
            shape.max_output_tokens.unwrap_or(0),
        );

        // --- 3. reservation: the cap holds BEFORE the spending ---
        let reservation = match cap.reserve(estimate, now) {
            Ok(p) => p,
            Err(ReserveError::Exceeded(e)) => {
                tracing::warn!(
                    tenant = %tenant.id,
                    requested = e.requested,
                    available = e.available,
                    "budget exhausted: 429 without calling any provider"
                );
                self.config.meter.record_budget_denied(&tenant.id);
                return budget_denied();
            }
            Err(ReserveError::State(_)) => {
                // the cap state is compromised. We **move on**: it is a gateway problem,
                // not a client one, and blocking every request because of a poisoned lock
                // would make things worse
                tracing::error!(tenant = %tenant.id, "budget state not readable: serving without the cap");
                return self
                    .without_reservation(&tenant.id, &model, &shape, now)
                    .await;
            }
        };

        // --- 4. forwarding and settlement ---
        let outcome = self.forward(&tenant.id, &model, &shape, request.body).await;

        match outcome {
            RouteOutcome::Served(routed) => {
                let (status, body, usage) = from_response(&routed.response);
                cap.settle(reservation, usage_cost(&usage, price), now);
                self.config.meter.record(
                    &tenant.id,
                    &shape,
                    usage,
                    &Outcome::Served {
                        provider: routed.provider.clone(),
                        price_estimated: estimated,
                    },
                );
                to_response(status, body, routed)
            }
            RouteOutcome::Failed(error) => {
                // no provider answered: the reservation was not spent, and it must be
                // released because the money did not go out
                cap.release(reservation, now);
                self.config.meter.record(
                    &tenant.id,
                    &shape,
                    Usage::default(),
                    &Outcome::Failed {
                        provider: error.to_string(),
                    },
                );
                tracing::warn!(tenant = %tenant.id, error = %error, "no provider served the request");
                Self::response_from_error(&error)
            }
        }
    }

    /// The path when the cap state is not readable: it is served without the cap, and it
    /// says so. It is the route ADR 0004 calls "the accountant does not bring the service
    /// down", applied to the cap instead of the accounting.
    async fn without_reservation(
        &self,
        tenant: &str,
        model: &str,
        shape: &RequestShape,
        now: i64,
    ) -> GatewayResponse {
        let _ = now;
        match self.forward(tenant, model, shape, Vec::new()).await {
            RouteOutcome::Served(routed) => {
                self.config.meter.record_uncovered(tenant);
                let (status, body, usage) = from_response(&routed.response);
                let (_, estimated) = self.config.meter.price_for(model);
                self.config.meter.record(
                    tenant,
                    shape,
                    usage,
                    &Outcome::Served {
                        provider: routed.provider.clone(),
                        price_estimated: estimated,
                    },
                );
                to_response(status, body, routed)
            }
            RouteOutcome::Failed(e) => Self::response_from_error(&e),
        }
    }

    /// Forwards to the router. It is not inside `handle` for one reason only: the
    /// authentication attempt and the forwarding attempt have nothing in common, and
    /// mixing them would make `handle` unreadable.
    async fn forward(
        &self,
        tenant: &str,
        model: &str,
        shape: &RequestShape,
        body: Vec<u8>,
    ) -> RouteOutcome {
        let _ = tenant;
        let _ = shape;
        let request = UpstreamRequest::new(body, model.to_owned(), shape.stream);
        match self.config.router.route(request).await {
            Ok(routed) => RouteOutcome::Served(routed),
            Err(e) => RouteOutcome::Failed(e),
        }
    }

    /// A response built by the gateway, not by a provider.
    fn from_gateway(status: u16, body: Vec<u8>, provider: Option<String>) -> GatewayResponse {
        GatewayResponse::Whole {
            status,
            body,
            provider,
        }
    }

    /// A gateway error translated into an HTTP response.
    fn reject(error: AuthError) -> GatewayResponse {
        match error {
            // no key: it says it is missing, and it also says how to solve that
            AuthError::Missing => Self::from_gateway(
                401,
                br#"{"error":{"message":"missing API key","type":"authentication_error"}}"#
                    .to_vec(),
                None,
            ),
            AuthError::Unknown => Self::from_gateway(
                401,
                br#"{"error":{"message":"invalid API key","type":"authentication_error"}}"#
                    .to_vec(),
                None,
            ),
            AuthError::Empty => Self::from_gateway(
                401,
                br#"{"error":{"message":"empty API key","type":"authentication_error"}}"#.to_vec(),
                None,
            ),
        }
    }

    /// A router error translated into a response, without revealing that a second
    /// provider exists.
    fn response_from_error(error: &RouteError) -> GatewayResponse {
        match error {
            // a provider error goes back to the client as it is: it is its error, and its
            // body explains why better than a response written by the gateway
            RouteError::ClientFault { status, body, .. }
            | RouteError::UnknownStatus { status, body, .. } => {
                Self::from_gateway(*status, body.clone(), None)
            }
            // the provider could not, and neither could the next one: it is a gateway fact
            RouteError::ProviderUnavailable { .. } => Self::from_gateway(
                502,
                br#"{"error":{"message":"all providers failed","type":"api_error"}}"#.to_vec(),
                None,
            ),
            RouteError::NoProviderForModel { model, available } => {
                tracing::info!(model = %model, ?available, "no provider for the model");
                Self::from_gateway(
                    404,
                    format!(
                        r#"{{"error":{{"message":"no provider serves model {model}","type":"not_found_error"}}}}"#
                    )
                    .into_bytes(),
                    None,
                )
            }
            RouteError::NoProviders => Self::from_gateway(
                503,
                br#"{"error":{"message":"gateway has no providers configured"}}"#.to_vec(),
                None,
            ),
            // here it does not say "I tried three providers": a client cannot do anything
            // with that information, and an attacker would make a map out of it
            RouteError::DeliveryUnknown { .. } | RouteError::Exhausted { .. } => {
                Self::from_gateway(
                    502,
                    br#"{"error":{"message":"all providers failed","type":"api_error"}}"#.to_vec(),
                    None,
                )
            }
        }
    }
}

/// The response to an exhausted cap.
///
/// The important thing here is not the body but the fact that **no provider was called**:
/// it is the difference between a cap and a report. A `429` after the call would have
/// already spent the money it was supposed to prevent spending.
fn budget_denied() -> GatewayResponse {
    GatewayResponse::Whole {
        status: 429,
        body: br#"{"error":{"message":"monthly budget exhausted","type":"rate_limit_error"}}"#
            .to_vec(),
        provider: None,
    }
}

/// The body of a provider, with the usage it contains.
fn from_response(response: &crate::upstream::UpstreamResponse) -> (u16, Vec<u8>, Usage) {
    let status = response.status;
    match &response.body {
        ResponseBody::Buffered(b) => (status, b.clone(), parse_usage(b)),
        // on a stream the usage arrives in the last chunk and is not available here:
        // that is why metering a stream must be done downstream
        ResponseBody::Stream(_) => (status, Vec::new(), Usage::default()),
    }
}

/// The cost of the real consumption, given the price already resolved.
fn usage_cost(usage: &Usage, price: crate::pricing::Price) -> MicroUsd {
    usage.cost(price)
}

/// The usage declared by the provider.
///
/// If the provider does not send `usage`, zero is counted. The gateway does **not**
/// estimate after the fact: a number invented after the fact is worse than a declared
/// zero, and in both cases the provider's invoice remains the authoritative source.
fn parse_usage(body: &[u8]) -> Usage {
    match serde_json::from_slice::<RawUsage>(body) {
        Ok(u) => {
            let usage = u.usage.unwrap_or_default();
            Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            }
        }
        Err(_) => Usage::default(),
    }
}

/// The shape in which the provider declares usage. The names are its own, not ours:
/// `Usage` has `input_tokens`/`output_tokens` because those are better names, and the
/// translation lives here and not in the type the rest of the gateway uses.
#[derive(serde::Deserialize)]
struct RawUsage {
    #[serde(default)]
    usage: Option<RawUsageBody>,
}

#[derive(Default, serde::Deserialize)]
struct RawUsageBody {
    #[serde(default, rename = "prompt_tokens")]
    prompt_tokens: u64,
    #[serde(default, rename = "completion_tokens")]
    completion_tokens: u64,
}

/// The router response, or the reason there was none.
enum RouteOutcome {
    Served(Routed),
    Failed(RouteError),
}

/// Converts the router response into the gateway one, preserving streaming.
fn to_response(status: u16, body: Vec<u8>, routed: Routed) -> GatewayResponse {
    match routed.response.body {
        ResponseBody::Buffered(_) => GatewayResponse::Whole {
            status,
            body,
            provider: Some(routed.provider),
        },
        ResponseBody::Stream(chunks) => GatewayResponse::Stream {
            status,
            chunks,
            provider: routed.provider,
        },
    }
}
