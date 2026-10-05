// This module is compiled twice, once per test binary that includes it.
// Each copy sees only the tools its own test uses, and would flag the others as dead.
// It is a shared toolbox: the unused half is normal.
#![allow(dead_code)]

//! A fake provider, to test the router without a network.
//!
//! This is where the project gains from `Upstream` being a trait: the whole failover
//! policy — the three error classes, the attempt cap, the ban on retrying a request that
//! already went out — is verified in microseconds and without a provider in between
//! having changed its answer, which is the problem with tests that hit a real API.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use llmgateway::upstream::{
    BoxFuture, TransportError, TransportKind, Upstream, UpstreamRequest, UpstreamResponse,
};

/// What the fake provider must do.
#[derive(Debug, Clone)]
pub enum Behavior {
    /// Responds with this status and this body.
    Responds(u16, String),
    /// Produces no response, and the request **did not go out**.
    NotSent(TransportKind),
    /// Produces no response, and the request **did go out**.
    SentWithoutResponse(TransportKind),
}

impl Behavior {
    /// Responds `200` with a JSON body.
    #[must_use]
    pub fn ok(body: &str) -> Self {
        Self::Responds(200, body.to_owned())
    }

    /// Responds `503`.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::Responds(503, "{\"error\":\"service unavailable\"}".to_owned())
    }
}

/// A provider that does exactly what it is told, and counts how many times.
#[derive(Debug)]
pub struct Fake {
    name: String,
    models: Option<Vec<String>>,
    behaviors: Mutex<Vec<Behavior>>,
    calls: AtomicUsize,
    last_models: Mutex<Vec<String>>,
}

impl Fake {
    /// A provider that always responds `200` to any model.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            models: None,
            behaviors: Mutex::new(vec![Behavior::ok("{\"ok\":true}")]),
            calls: AtomicUsize::new(0),
            last_models: Mutex::new(Vec::new()),
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
    pub fn with_behaviors(self, behaviors: Vec<Behavior>) -> Self {
        *self.behaviors.lock().expect("behaviors lock") = behaviors;
        self
    }

    /// How many times it was called.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The models it was asked for, in order.
    #[must_use]
    pub fn requested_models(&self) -> Vec<String> {
        self.last_models.lock().expect("models lock").clone()
    }

    fn next(&self) -> Behavior {
        let mut list = self.behaviors.lock().expect("behaviors lock");
        if list.len() == 1 {
            return list[0].clone();
        }
        list.remove(0)
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_models
            .lock()
            .expect("models lock")
            .push(request.model.clone());

        // everything the future uses is **copied**: the future lives longer than the
        // borrow of `&self`, and keeping the reference in there would not compile
        let behavior = self.next();
        let name = self.name.clone();
        let stream = request.stream;

        llmgateway::upstream::boxed(move || async move {
            let name = name.as_str();
            match behavior {
                Behavior::Responds(200, body) if stream => {
                    // streaming arrives in pieces: the gateway must forward it without
                    // waiting for the last one
                    let chunks: Vec<Vec<u8>> = body
                        .lines()
                        .map(|l| format!("data: {l}\n\n").into_bytes())
                        .collect();
                    Ok(UpstreamResponse::streaming(200, chunks))
                }
                Behavior::Responds(status, body) => Ok(UpstreamResponse::buffered(status, body)),
                Behavior::NotSent(kind) => Err(TransportError::not_sent(
                    kind,
                    format!("{name} unreachable"),
                )),
                Behavior::SentWithoutResponse(kind) => Err(TransportError::maybe_sent(
                    kind,
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
