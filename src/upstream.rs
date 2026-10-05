//! The abstraction over the provider: what an upstream can do, and what can go wrong.
//!
//! `Upstream` is a trait, and it is one for a reason that is not testability (which is a
//! side effect): if the provider is an interface, **failover is domain logic**, not HTTP
//! code. The error classification in ADR 0003, the budget reservation, the cap on
//! attempts: all of that sits above the interface and knows nothing about `reqwest`.
//!
//! The most delicate point of the file is [`Delivery`]. When a call fails, the question
//! that matters is not "which error is it" but **"did the request get out?"**:
//!
//! - if it did not get out, retrying on another provider is safe: nobody executed it;
//! - if it got out and we do not know whether it was executed, retrying can **do the
//!   same operation twice**. An error is better than a double charge.
//!
//! A timeout and a refused connection look like the same error and are not.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::Stream;

use crate::request::Bytes;

/// A future that can go behind a `dyn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A stream of chunks: the provider SSE stream, without buffering it.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send>>;

/// A request towards a provider.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    /// The body as it is, byte for byte.
    pub body: Bytes,
    /// The requested model, for routing.
    pub model: String,
    /// `true` if the client asked for streaming.
    pub stream: bool,
}

impl UpstreamRequest {
    /// Builds a request from the shape read from the incoming body.
    #[must_use]
    pub fn new(body: Bytes, model: impl Into<String>, stream: bool) -> Self {
        Self {
            body,
            model: model.into(),
            stream,
        }
    }
}

/// The body of a response: whole, or in progress.
pub enum ResponseBody {
    /// All in memory. The normal path.
    Buffered(Bytes),
    /// Still in progress. The gateway forwards it without accumulating it: accumulating
    /// a stream means making the first token wait until the last one arrives, that is,
    /// taking away from the client the only thing streaming exists for.
    Stream(ByteStream),
}

/// `Debug` written by hand: the payload of a stream is not formattable, and a `{:?}` that
/// prints the whole chunks would end up in a log with the provider's response inside.
/// Here what is printed is **which shape** the body has, not its content — which is what
/// is needed to understand an error.
impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buffered(b) => f
                .debug_tuple("Buffered")
                .field(&format_args!("{} bytes", b.len()))
                .finish(),
            Self::Stream(_) => f.write_str("Stream(<in progress>)"),
        }
    }
}

impl ResponseBody {
    /// `true` if it is a stream.
    #[must_use]
    pub fn is_stream(&self) -> bool {
        matches!(self, Self::Stream(_))
    }
}

/// A response that arrived. It can still be an error.
#[derive(Debug)]
pub struct UpstreamResponse {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: ResponseBody,
}

impl UpstreamResponse {
    /// An in-memory response, convenient for tests and for providers that do not stream.
    #[must_use]
    pub fn buffered(status: u16, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            body: ResponseBody::Buffered(body.into()),
        }
    }

    /// A streaming response.
    #[must_use]
    pub fn streaming(status: u16, chunks: Vec<Bytes>) -> Self {
        Self {
            status,
            body: ResponseBody::Stream(Box::pin(futures_util::stream::iter(
                chunks.into_iter().map(Ok),
            ))),
        }
    }
}

/// Whether the request got out, and what can be done about it.
///
/// It is the distinction that separates "retry" from "do not retry" (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The request **did not reach** the provider: connection refused, failed DNS,
    /// missing route. Retrying elsewhere is safe.
    NotSent,
    /// The request **got out** and it is not known whether it was executed: timeout
    /// after sending, connection dropped halfway. Retrying can cost twice.
    Unknown,
}

impl Delivery {
    /// `true` if it is safe to move to another provider without risking a double charge.
    #[must_use]
    pub fn safe_to_retry(self) -> bool {
        matches!(self, Self::NotSent)
    }
}

/// Why a call produced no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Connection refused, DNS, missing route: nothing got out.
    Connect,
    /// Timeout.
    Timeout,
    /// Connection dropped after sending.
    Reset,
    /// TLS, DNS and everything else that is neither connection nor timeout.
    Other,
}

/// A transport error: no response arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// What nature it is.
    pub kind: TransportKind,
    /// Whether the request got out. **It cannot be inferred from the error kind.**
    pub delivery: Delivery,
    /// A descriptive line, for logs. It never ends up in a response to the client.
    pub detail: String,
}

impl TransportError {
    /// An error where the request did not get out: it can be retried elsewhere.
    #[must_use]
    pub fn not_sent(kind: TransportKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            delivery: Delivery::NotSent,
            detail: detail.into(),
        }
    }

    /// An error where the request got out and it is not known: it is **not** retried.
    #[must_use]
    pub fn maybe_sent(kind: TransportKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            delivery: Delivery::Unknown,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} ({:?}): {}", self.kind, self.delivery, self.detail)
    }
}

impl std::error::Error for TransportError {}

/// Whose fault a response that is an error was.
///
/// Three classes, and the third one is the one people forget (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The provider's fault: move on to the next one.
    Retryable,
    /// The client's fault: do not retry, return it. A `400` replayed on another provider
    /// becomes a `400` after a latency the user has already seen.
    ClientFault,
    /// We do not know what it is. It is **not** retried and it is reported: an error that
    /// was not understood, amplified, is worse than an error propagated.
    Unknown,
}

/// Classifies an HTTP status.
///
/// The rule is short to remember: **4xx is the client's, 5xx is the provider's**, and
/// `429` is on the provider side even though it is a 4xx, because it is a rate limit and
/// the next provider is exactly the right answer.
#[must_use]
pub fn classify(status: u16) -> FailureClass {
    match status {
        // 408 and 429 are the provider's even though they are 4xx: they are timeouts and
        // rate limits, and the next provider is exactly the right answer
        408 | 429 | 500..=599 => FailureClass::Retryable,
        400..=499 => FailureClass::ClientFault,
        _ => FailureClass::Unknown,
    }
}

/// A provider.
///
/// Object-safe by hand (`BoxFuture`) instead of with `async_trait`: it is the same thing
/// without a dependency, and ADR 0001 keeps the graph closed.
pub trait Upstream: Send + Sync {
    /// The name it appears under in logs and metrics.
    fn name(&self) -> &str;

    /// The models this provider serves. `None` means **all**: many providers answer to
    /// any model, and forcing them to list them would be a list to maintain that ages
    /// badly.
    fn models(&self) -> Option<&[String]>;

    /// `true` if this provider serves the requested model.
    ///
    /// A provider that does not serve it is **skipped**, not treated as failed: it is not
    /// an error, it is a routing choice, and counting it as an error would raise an alarm
    /// every time the model changes.
    fn supports(&self, model: &str) -> bool {
        match self.models() {
            None => true,
            Some(models) => models.iter().any(|m| m == model),
        }
    }

    /// Sends the request. It must not retry on its own: the attempt is a router
    /// decision, and the router knows the budget and the cap.
    fn send(
        &self,
        request: UpstreamRequest,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<UpstreamResponse, TransportError>>;
}

/// How to build an `Upstream` by hand, without writing the `Box::pin` by hand.
pub fn boxed<F, Fut, T>(fut: F) -> BoxFuture<'static, T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    Box::pin(fut())
}

/// A shared provider.
pub type SharedUpstream = Arc<dyn Upstream>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_4xx_is_the_client_and_a_5xx_the_provider() {
        assert_eq!(classify(400), FailureClass::ClientFault);
        assert_eq!(classify(404), FailureClass::ClientFault);
        assert_eq!(classify(422), FailureClass::ClientFault);
        assert_eq!(classify(500), FailureClass::Retryable);
        assert_eq!(classify(503), FailureClass::Retryable);
    }

    #[test]
    fn a_429_is_the_provider_even_though_it_is_a_4xx() {
        // it is a rate limit: the next provider is the right answer
        assert_eq!(classify(429), FailureClass::Retryable);
        assert_eq!(classify(408), FailureClass::Retryable);
    }

    #[test]
    fn a_status_that_is_neither_the_client_nor_the_provider_is_not_retryable() {
        // a 3xx the gateway does not route, a 1xx, a 6xx: the gateway does not know what
        // they are, and doing nothing is more honest than amplifying them on three
        // providers
        for s in [100, 101, 301, 302, 600, 999] {
            assert_eq!(classify(s), FailureClass::Unknown, "status {s}");
        }
    }

    #[test]
    fn a_4xx_we_do_not_understand_stops_failover_like_every_other_client_error() {
        // 402 and 451 are not errors that get solved by retrying: the client does not pay
        // and the request is unlawful. They are not "unknown statuses", they are the
        // client's like a 400 — and the router stops in both cases
        for s in [402, 451, 413, 422] {
            assert_eq!(classify(s), FailureClass::ClientFault, "status {s}");
        }
    }

    #[test]
    fn only_what_did_not_get_out_is_retried() {
        assert!(Delivery::NotSent.safe_to_retry());
        assert!(!Delivery::Unknown.safe_to_retry());
    }

    #[test]
    fn a_timeout_after_sending_is_not_as_safe_as_a_connection_refusal() {
        // the same visible symptom, two different facts
        let refused = TransportError::not_sent(TransportKind::Connect, "connection refused");
        let timed_out = TransportError::maybe_sent(TransportKind::Timeout, "timeout");
        assert!(refused.delivery.safe_to_retry());
        assert!(!timed_out.delivery.safe_to_retry());
    }

    #[test]
    fn the_body_of_a_response_knows_whether_it_is_streaming() {
        assert!(!UpstreamResponse::buffered(200, "{}").body.is_stream());
        assert!(UpstreamResponse::streaming(200, vec![]).body.is_stream());
    }

    #[test]
    fn the_transport_error_prints_without_highlighting_the_details() {
        let e = TransportError::maybe_sent(TransportKind::Reset, "connection dropped");
        assert!(e.to_string().contains("Reset"));
        assert!(e.to_string().contains("connection dropped"));
    }
}
