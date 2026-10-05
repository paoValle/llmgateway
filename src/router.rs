//! The router: it picks the provider and applies failover.
//!
//! All the policy is here and it **knows nothing about HTTP**: `Upstream` is a trait,
//! `TransportError` is an enum. It is the direct consequence of ADR 0001 and ADR 0003 —
//! the rules that decide are economic and domain rules, so the place where they live has
//! nothing to do with transport.
//!
//! The algorithm, in one line per attempt:
//!
//! - the provider **does not serve** the model → **skipped**, it is not an error;
//! - the response is a success → returned, with the name of who served it;
//! - the error is the **client's** or **unknown** → returned immediately, no failover;
//! - the error is the **provider's** or a transport one → on to the next, if any are
//!   left and if retrying is safe.
//!
//! That last "if retrying is safe" is [`Upstream::Delivery`]: a request that got out
//! with an unknown outcome is not retried, because a double charge costs more than an
//! error.

use std::time::Duration;

use tracing::{debug, info, warn};

use crate::upstream::{
    classify, Delivery, FailureClass, ResponseBody, SharedUpstream, TransportError, TransportKind,
    Upstream, UpstreamRequest, UpstreamResponse,
};

/// Cap on attempts across all providers, if the caller does not specify one.
///
/// Failover without a cap is a self-feeding attack: when the providers are slow for one
/// another, every attempt adds load exactly when there is already too much.
pub const DEFAULT_MAX_ATTEMPTS: usize = 4;

/// Why the router could not serve the request.
#[derive(Debug)]
pub enum RouteError {
    /// No provider claims to serve the requested model.
    NoProviderForModel {
        /// The requested model.
        model: String,
        /// The models the available providers really serve.
        available: Vec<String>,
    },
    /// There is no configured provider.
    NoProviders,
    /// A provider answered with a **client** error: it is returned as it is.
    ClientFault {
        /// Who answered.
        provider: String,
        /// The status.
        status: u16,
        /// The response body, to forward it.
        body: Vec<u8>,
    },
    /// All providers answered "I cannot", or the last attempt ended that way. It is
    /// **not** a client error and it is not the status of a single provider: it is the
    /// fact that the gateway, as a whole, could not serve. It must be reported as `502`,
    /// not by forwarding the provider's status.
    ProviderUnavailable {
        /// The last provider that answered.
        provider: String,
        /// The status it gave.
        status: u16,
    },
    /// A provider answered with a status the gateway cannot classify.
    UnknownStatus {
        /// Who answered.
        provider: String,
        /// The status.
        status: u16,
        /// The body, to forward it.
        body: Vec<u8>,
    },
    /// The call failed and the request had already gone out: it is **not** retried.
    DeliveryUnknown {
        /// The provider that could not do it.
        provider: String,
        /// The error.
        error: TransportError,
    },
    /// All attempts ran out.
    Exhausted {
        /// How many attempts were made.
        attempts: usize,
        /// The last error encountered, for the message to the client.
        last: Box<RouteError>,
    },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProviderForModel { model, available } => write!(
                f,
                "no provider serves model {model:?}; available: {}",
                if available.is_empty() { "(none)".to_owned() } else { available.join(", ") }
            ),
            Self::NoProviders => f.write_str("no configured provider"),
            Self::ClientFault { provider, status, .. } => {
                write!(f, "{provider} rejected the request ({status}): client error, it is not retried")
            }
            Self::ProviderUnavailable { provider, status, .. } => write!(
                f,
                "no provider could serve the request (last: {provider}, {status})"
            ),
            Self::UnknownStatus { provider, status, .. } => write!(
                f,
                "{provider} answered {status}, a status the gateway cannot classify: it is not retried"
            ),
            Self::DeliveryUnknown { provider, error } => write!(
                f,
                "{provider}: {error} — the request had already gone out, it is not retried to avoid a double charge"
            ),
            Self::Exhausted { attempts, last } => write!(f, "{attempts} attempts exhausted ({last})"),
        }
    }
}

impl std::error::Error for RouteError {}

/// A served response, with the name of who served it.
#[derive(Debug)]
pub struct Routed {
    /// Who answered.
    pub provider: String,
    /// How many attempts it took.
    pub attempts: usize,
    /// The response.
    pub response: UpstreamResponse,
}
/// The router.
#[derive(Clone)]
pub struct Router {
    providers: Vec<SharedUpstream>,
    max_attempts: usize,
    timeout: Duration,
}

/// `Debug` by hand: the providers are trait objects, and a `{:?}` printing their
/// internals would end up in a log. Here what is visible is **which** providers and with
/// what cap, which is the question asked during an incident.
impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field(
                "providers",
                &self.providers.iter().map(|p| p.name()).collect::<Vec<_>>(),
            )
            .field("max_attempts", &self.max_attempts)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Router {
    /// Creates a router. `max_attempts` of zero becomes [`DEFAULT_MAX_ATTEMPTS`]: a cap of
    /// zero is not "no attempt", it is a gateway that serves nobody.
    #[must_use]
    pub fn new(providers: Vec<SharedUpstream>, max_attempts: usize, timeout: Duration) -> Self {
        Self {
            providers,
            max_attempts: if max_attempts == 0 {
                DEFAULT_MAX_ATTEMPTS
            } else {
                max_attempts
            },
            timeout,
        }
    }

    /// The providers, in order of preference.
    #[must_use]
    pub fn providers(&self) -> &[SharedUpstream] {
        &self.providers
    }

    /// The cap on attempts.
    #[must_use]
    pub fn max_attempts(&self) -> usize {
        self.max_attempts
    }

    /// The models declared by some provider, for a useful error message.
    ///
    /// A provider that serves **all** models does not appear: in the error "nobody serves
    /// this model" what is needed is the list of the models that are **not** available,
    /// and an "all models" adds nothing.
    #[must_use]
    pub fn servable_models(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .providers
            .iter()
            .filter_map(|p| p.models())
            .flatten()
            .cloned()
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// Sends the request, with failover.
    pub async fn route(&self, request: UpstreamRequest) -> Result<Routed, RouteError> {
        if self.providers.is_empty() {
            return Err(RouteError::NoProviders);
        }

        let candidates: Vec<&SharedUpstream> = self
            .providers
            .iter()
            .filter(|p| p.supports(&request.model))
            .collect();

        if candidates.is_empty() {
            return Err(RouteError::NoProviderForModel {
                model: request.model.clone(),
                available: self.servable_models(),
            });
        }

        let mut attempts = 0usize;
        let mut last: Option<RouteError> = None;

        for provider in candidates {
            if attempts >= self.max_attempts {
                debug!(attempts, "attempt cap reached");
                break;
            }

            attempts += 1;
            match self.attempt(provider.as_ref(), &request).await {
                Attempt::Served(response) => {
                    return Ok(Routed {
                        provider: provider.name().to_owned(),
                        attempts,
                        response,
                    });
                }
                Attempt::Continue(error) => last = Some(error),
                Attempt::Stop(error) => return Err(error),
            }
        }

        Err(match last {
            Some(last) if attempts >= self.max_attempts => RouteError::Exhausted {
                attempts,
                last: Box::new(last),
            },
            Some(last) => last,
            None => RouteError::Exhausted {
                attempts,
                last: Box::new(RouteError::NoProviders),
            },
        })
    }

    /// One attempt on one provider, and what it implies for the following ones.
    ///
    /// This is where the whole policy of ADR 0003 lives, and it is a separate function
    /// because reading it whole makes the difference between "continue" and "stop"
    /// evident.
    async fn attempt(&self, provider: &dyn Upstream, request: &UpstreamRequest) -> Attempt {
        let name = provider.name();

        let response = match provider.send(request.clone(), self.timeout).await {
            Ok(r) => r,
            Err(error) => return on_transport(name, error),
        };

        if (200..300).contains(&response.status) {
            return Attempt::Served(response);
        }

        match classify(response.status) {
            FailureClass::Retryable => {
                warn!(
                    provider = name,
                    status = response.status,
                    "provider error, moving on"
                );
                Attempt::Continue(classify_response(name, response.status, body_of(&response)))
            }
            FailureClass::ClientFault => {
                info!(
                    provider = name,
                    status = response.status,
                    "client error: no failover"
                );
                Attempt::Stop(classify_response(name, response.status, body_of(&response)))
            }
            FailureClass::Unknown => {
                // amplifying an error you do not understand is worse than propagating it
                warn!(
                    provider = name,
                    status = response.status,
                    "unclassified status: no failover on an error that was not understood"
                );
                Attempt::Stop(classify_response(name, response.status, body_of(&response)))
            }
        }
    }
}

/// The outcome of an attempt, and what it implies for the next one.
#[derive(Debug)]
enum Attempt {
    /// Good response: the round is over.
    Served(UpstreamResponse),
    /// Provider error, or request that never left: try the next one.
    Continue(RouteError),
    /// Client error, unknown status, or request already sent: stop.
    Stop(RouteError),
}

/// What to do when a provider gave no response.
///
/// The question is not "which error is it" but **"did the request get out?"**: if it did
/// not, we continue; if it did and we do not know whether it was executed, we do not —
/// a double charge costs more than an error (ADR 0003).
fn on_transport(provider: &str, error: TransportError) -> Attempt {
    if error.delivery == Delivery::NotSent {
        warn!(
            provider,
            kind = ?error.kind,
            detail = %error.detail,
            "no response, the request had not gone out: moving on"
        );
        return Attempt::Continue(RouteError::DeliveryUnknown {
            provider: provider.to_owned(),
            error,
        });
    }

    warn!(
        provider,
        kind = ?error.kind,
        detail = %error.detail,
        "the request had already gone out: no failover, to avoid a double charge"
    );
    Attempt::Stop(RouteError::DeliveryUnknown {
        provider: provider.to_owned(),
        error,
    })
}

/// Turns an error response into its `RouteError`, without losing its class.
fn classify_response(provider: &str, status: u16, body: Vec<u8>) -> RouteError {
    match classify(status) {
        FailureClass::ClientFault => RouteError::ClientFault {
            provider: provider.to_owned(),
            status,
            body,
        },
        FailureClass::Retryable => RouteError::ProviderUnavailable {
            provider: provider.to_owned(),
            status,
        },
        FailureClass::Unknown => RouteError::UnknownStatus {
            provider: provider.to_owned(),
            status,
            body,
        },
    }
}

/// The body of a response, if it is in memory. On a stream there is none: and that is
/// why metering a stream must be done downstream, by the client.
fn body_of(response: &UpstreamResponse) -> Vec<u8> {
    match &response.body {
        ResponseBody::Buffered(b) => b.clone(),
        ResponseBody::Stream(_) => Vec::new(),
    }
}

/// Builds the "request did not go out" error, for whoever implements a provider and does
/// not want to build the error by hand.
#[must_use]
pub fn error_not_sent(kind: TransportKind, detail: &str) -> TransportError {
    TransportError::not_sent(kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_router_has_zero_providers() {
        let r = Router::new(vec![], 4, Duration::from_secs(1));
        assert_eq!(r.max_attempts(), 4);
        assert!(r.providers().is_empty());
    }

    #[test]
    fn a_cap_of_zero_becomes_the_default() {
        let r = Router::new(vec![], 0, Duration::from_secs(1));
        assert_eq!(r.max_attempts(), DEFAULT_MAX_ATTEMPTS);
    }

    #[test]
    fn the_transport_error_prints_on_one_line() {
        let e = RouteError::DeliveryUnknown {
            provider: "a".to_owned(),
            error: TransportError::maybe_sent(TransportKind::Timeout, "timeout after sending"),
        };
        let text = e.to_string();
        assert!(text.contains("double charge"));
        assert!(
            !text.contains('\n'),
            "a log message must not wrap to a new line"
        );
    }

    #[test]
    fn a_model_nobody_serves_says_so_with_an_empty_list() {
        let e = RouteError::NoProviderForModel {
            model: "x".to_owned(),
            available: vec![],
        };
        assert!(e.to_string().contains("(none)"));
    }

    #[test]
    fn the_router_debug_shows_the_providers_and_the_cap() {
        let r = Router::new(vec![], 7, Duration::from_millis(250));
        let text = format!("{r:?}");
        assert!(text.contains("max_attempts: 7"));
        assert!(text.contains("250"));
    }
}
