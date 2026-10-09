// This module is compiled twice, once per test binary that includes it.
// Each copy sees only the tools its own test uses, and would flag the others as dead.
// It is a shared toolbox: the unused half is normal.
#![allow(dead_code)]

//! The adapter from `test-provider` onto `llmgateway`'s `Upstream`.
//!
//! The script, the counters and the JSON bodies live in `test-provider`: one fake provider for the
//! projects that test a gateway, so a behaviour added for one of them is not invisible in the
//! others. What cannot be shared is the glue — `Upstream`, `TransportError` and `TransportKind`
//! are `llmgateway`'s vocabulary, and a crate that owned them would have to be released in lockstep
//! with this one.
//!
//! This is where the project gains from `Upstream` being a trait: the whole failover
//! policy — the three error classes, the attempt cap, the ban on retrying a request that
//! already went out — is verified in microseconds and without a provider in between
//! having changed its answer, which is the problem with tests that hit a real API.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use llmgateway::upstream::{
    BoxFuture, TransportError, TransportKind, Upstream, UpstreamRequest, UpstreamResponse,
};
use test_provider::{Answer, Provider};

pub use test_provider::Behavior;

/// Responds `200` with this body, forwarded as it is. The name is `responds`, not `ok`,
/// because `Behavior::ok` (re-exported above) builds a usage body instead: two constructors
/// with the same name and different bodies is how a test passes for the wrong reason.
#[must_use]
pub fn responds(body: &str) -> Behavior {
    Behavior::Responds {
        status: 200,
        body: body.to_owned(),
    }
}

/// A provider that does exactly what it is told, and records the models it was asked for.
#[derive(Debug)]
pub struct Fake {
    name: String,
    models: Option<Vec<String>>,
    script: Provider,
    requested: Mutex<Vec<String>>,
    maybe_sent_kind: TransportKind,
}

impl Fake {
    /// A provider that always responds `200` to any model.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            models: None,
            script: Provider::new(vec![responds("{\"ok\":true}")]),
            requested: Mutex::new(Vec::new()),
            maybe_sent_kind: TransportKind::Timeout,
        }
    }

    /// A provider that serves only these models.
    #[must_use]
    pub fn with_models(name: &str, models: &[&str]) -> Self {
        let mut fake = Self::new(name);
        fake.models = Some(models.iter().map(|m| (*m).to_owned()).collect());
        fake
    }

    /// Sets the behaviors, one per attempt. The last one holds for all the following
    /// attempts: a provider that always responds `503` is written with a single element.
    #[must_use]
    pub fn with_behaviors(mut self, behaviors: Vec<Behavior>) -> Self {
        self.script = Provider::new(behaviors);
        self
    }

    /// The kind a `SentWithoutResponse` failure carries. The adapter is what chooses it — the
    /// router acts on `delivery`, not on the kind — and this is how a test pins one.
    #[must_use]
    pub fn failing_as(mut self, kind: TransportKind) -> Self {
        self.maybe_sent_kind = kind;
        self
    }

    /// How many times it was called.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.script.calls()
    }

    /// The models it was asked for, in order.
    #[must_use]
    pub fn requested_models(&self) -> Vec<String> {
        self.requested.lock().expect("models lock").clone()
    }
}

impl Upstream for Fake {
    fn name(&self) -> &str {
        &self.name
    }

    fn models(&self) -> Option<&[String]> {
        self.models.as_deref()
    }

    fn send(
        &self,
        request: UpstreamRequest,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<UpstreamResponse, TransportError>> {
        self.requested
            .lock()
            .expect("models lock")
            .push(request.model.clone());

        // the answer comes back owned, and the name is copied: the future lives longer than
        // the borrow of `&self`, and keeping the reference in there would not compile
        let answer = self.script.answer(&request.body);
        let name = self.name.clone();
        let stream = request.stream;
        let maybe_sent_kind = self.maybe_sent_kind;

        llmgateway::upstream::boxed(move || async move {
            let name = name.as_str();
            match answer {
                // streaming arrives in pieces: the gateway must forward it without
                // waiting for the last one
                Answer::Responded { status: 200, body } if stream => {
                    let chunks: Vec<Vec<u8>> = body
                        .lines()
                        .map(|l| format!("data: {l}\n\n").into_bytes())
                        .collect();
                    Ok(UpstreamResponse::streaming(200, chunks))
                }
                Answer::Responded { status, body } => Ok(UpstreamResponse::buffered(status, body)),
                Answer::Streamed { status, chunks } => Ok(UpstreamResponse::streaming(
                    status,
                    chunks.into_iter().map(String::into_bytes).collect(),
                )),
                // the kind is this adapter's choice: what the router acts on is `delivery`, and
                // the two failure modes are the two values of it
                Answer::NotSent => Err(TransportError::not_sent(
                    TransportKind::Connect,
                    format!("{name} unreachable"),
                )),
                Answer::SentWithoutResponse => Err(TransportError::maybe_sent(
                    maybe_sent_kind,
                    format!("{name} died after receiving the request"),
                )),
            }
        })
    }
}

/// A shared provider, ready for the router.
#[must_use]
pub fn shared(fake: Fake) -> Arc<dyn Upstream> {
    Arc::new(fake)
}
