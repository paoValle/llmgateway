//! The provider side: an `Upstream` that really speaks HTTP.
//!
//! Everything else in this crate decides *whether* to call a provider; this is the only
//! place that calls one. It is deliberately small, and the reason it is a separate file is
//! the error mapping at the bottom: turning a `reqwest::Error` into
//! [`Delivery::NotSent`] or [`Delivery::Unknown`] is the decision that prevents a double
//! charge, and it deserves to be readable in isolation (ADR 0003).
//!
//! The body is forwarded **byte for byte**: no re-serialization, no field the client knows
//! and this gateway does not.

use std::time::Duration;

use futures_util::StreamExt;

use crate::request::Bytes;
use crate::upstream::{
    boxed, BoxFuture, ResponseBody, TransportError, TransportKind, Upstream, UpstreamRequest,
    UpstreamResponse,
};

/// A provider reachable over HTTP, OpenAI-compatible.
#[derive(Debug)]
pub struct HttpUpstream {
    name: String,
    base_url: String,
    api_key: String,
    models: Option<Vec<String>>,
    client: reqwest::Client,
}

impl HttpUpstream {
    /// Builds a provider. `models` is `None` for "serves everything".
    ///
    /// The client is built once and reused: connection pooling is the reason a gateway can
    /// be faster than the client talking to the provider directly, and building a client per
    /// request would throw that away.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        models: Option<Vec<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            models,
            client: reqwest::Client::new(),
        }
    }
}

impl Upstream for HttpUpstream {
    fn name(&self) -> &str {
        &self.name
    }

    fn models(&self) -> Option<&[String]> {
        self.models.as_deref()
    }

    fn send(
        &self,
        request: UpstreamRequest,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<UpstreamResponse, TransportError>> {
        // everything is cloned: the future outlives the borrow of `&self`
        let client = self.client.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let key = self.api_key.clone();
        let body: Bytes = request.body;
        let stream = request.stream;

        boxed(move || async move {
            let sent = client
                .post(&url)
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(timeout)
                .body(body)
                .send()
                .await
                .map_err(map_error)?;

            let status = sent.status().as_u16();

            if stream {
                // forwarded chunk by chunk: buffering a stream here would give the client the
                // last byte before the first one, which is the opposite of streaming
                let chunks = sent
                    .bytes_stream()
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(map_error));
                return Ok(UpstreamResponse {
                    status,
                    body: ResponseBody::Stream(Box::pin(chunks)),
                });
            }

            let bytes = sent.bytes().await.map_err(map_error)?;
            Ok(UpstreamResponse::buffered(status, bytes.to_vec()))
        })
    }
}

/// Which failures may be retried on another provider.
///
/// The rule is the whole point of [`Delivery`], and it is not "what kind of error is it"
/// but **"did the request go out"**:
///
/// - a connection that never opened is `NotSent`: retrying elsewhere is safe;
/// - a timeout is `Unknown`: the request may have been executed, so retrying could charge
///   twice — an error is better than a double charge;
/// - a broken body *after* a response is `Unknown` for the same reason.
///
/// A `reqwest::Error` carries no field that says "this one left", so the classification is
/// conservative on purpose: only the connection phase counts as not-sent.
// Taking the error by value is not "needless": it is what `Result::map_err` requires
// without wrapping every call site in a closure that only forwards it.
#[allow(clippy::needless_pass_by_value)]
fn map_error(error: reqwest::Error) -> TransportError {
    let detail = error.to_string();
    if error.is_connect() {
        TransportError::not_sent(TransportKind::Connect, detail)
    } else if error.is_timeout() {
        TransportError::maybe_sent(TransportKind::Timeout, detail)
    } else if error.is_body() {
        TransportError::maybe_sent(TransportKind::Reset, detail)
    } else {
        TransportError::maybe_sent(TransportKind::Other, detail)
    }
}
