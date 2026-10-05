//! The incoming request, as seen by the gateway.
//!
//! The gateway does not understand what a prompt contains, and it must not. It does
//! have to know three things, and only three: **which model it is** (for the price and
//! for routing), **how much output is being requested** (to estimate the cost before
//! calling) and **whether it is streaming** (in order not to buffer).
//!
//! Everything else in the body is forwarded **byte for byte**. This is not a
//! simplification: a gateway that re-serializes a provider's body ends up losing the
//! fields it does not know about, and one day a new field arrives and disappears
//! without anyone noticing.
//!
//! The types are extracted from the JSON body without typing all of it: a
//! `serde_json::Value` of the whole request would cost a full parse for two strings.

use serde::Deserialize;

/// The bytes of an HTTP body.
pub type Bytes = Vec<u8>;

/// Bytes per token, the standard estimate for English texts.
///
/// Rounded up means it **never underestimates**: an Italian text or code uses more
/// tokens per character, and the divisor is fine for a cap, wrong for a quote.
/// See ADR 0002.
pub const BYTES_PER_TOKEN: usize = 4;

/// Maximum output assumed when the client does not declare it.
///
/// Deliberately generous: if the client does not say how much it wants to generate,
/// the worst is assumed. Underestimating here means the cap does not cover.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4_096;

/// How much the gateway understood of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestShape {
    /// The requested model. `None` if the body does not declare it in a readable way.
    pub model: Option<String>,
    /// The maximum output tokens declared by the client, if it does.
    pub max_output_tokens: Option<u64>,
    /// `true` if the client asked for streaming.
    pub stream: bool,
    /// The estimate of the input tokens.
    pub estimated_input_tokens: u64,
    /// The length of the body, which is what is charged for in bandwidth.
    pub body_bytes: usize,
}

/// The three fields the gateway must read from the body.
///
/// `deny_unknown_fields` is **not** there, and that is the choice: the body belongs to
/// the provider, not to the gateway, and rejecting it because it has one extra field
/// would mean the gateway must be updated every time the provider adds one.
#[derive(Debug, Deserialize)]
struct ModelAndLimits {
    model: Option<String>,
    max_tokens: Option<u64>,
    stream: Option<bool>,
}

/// Reads everything the gateway needs from the body, and **never raises**.
///
/// A body that is not JSON, or is JSON without `model`, is not a gateway error: it is a
/// request the provider will reject. Here we extract what we can, and leave the
/// diagnosis to whoever can answer (the provider, or the router if nobody can).
#[must_use]
pub fn inspect(body: &[u8], max_output_default: u64) -> RequestShape {
    let shape: Option<ModelAndLimits> = serde_json::from_slice(body).ok();

    RequestShape {
        model: shape.as_ref().and_then(|f| f.model.clone()),
        max_output_tokens: shape
            .as_ref()
            .and_then(|f| f.max_tokens)
            .filter(|n| *n > 0)
            .or(Some(max_output_default)),
        stream: shape.as_ref().and_then(|f| f.stream).unwrap_or(false),
        estimated_input_tokens: estimate_input_tokens(body.len()),
        body_bytes: body.len(),
    }
}

/// The input tokens estimated from the length of the body.
///
/// Rounded **up**: an estimate that underestimates is an estimate that lets expensive
/// requests through, and the cap stops covering.
#[must_use]
pub fn estimate_input_tokens(body_bytes: usize) -> u64 {
    body_bytes.div_ceil(BYTES_PER_TOKEN) as u64
}

impl RequestShape {
    /// The maximum amount this request can cost, given a price.
    ///
    /// It is the estimate reserved before calling: estimated input plus maximum output,
    /// both at the stated price. `None` without a model: without a model there is no
    /// price, and the cap cannot be computed.
    #[must_use]
    pub fn worst_case_cost(
        &self,
        price: crate::pricing::Price,
    ) -> Option<crate::pricing::MicroUsd> {
        let model = self.model.as_deref()?;
        let _ = model;
        Some(price.cost(self.estimated_input_tokens, self.max_output_tokens?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_model_and_max_output() {
        let body = br#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}],"max_tokens":256}"#;
        let shape = inspect(body, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(shape.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(shape.max_output_tokens, Some(256));
        assert!(!shape.stream);
    }

    #[test]
    fn without_max_tokens_the_worst_is_assumed() {
        // if the client does not declare it, the configured maximum is estimated:
        // underestimating here means the cap does not cover
        let shape = inspect(br#"{"model":"m"}"#, 8192);
        assert_eq!(shape.max_output_tokens, Some(8192));
    }

    #[test]
    fn max_tokens_zero_counts_as_undeclared() {
        // zero output tokens makes no sense in a real request: it is a client bug,
        // and the cap must not cover it as if it were a free request
        let shape = inspect(br#"{"model":"m","max_tokens":0}"#, 4096);
        assert_eq!(shape.max_output_tokens, Some(4096));
    }

    #[test]
    fn streaming_is_recognized() {
        assert!(inspect(br#"{"model":"m","stream":true}"#, 4096).stream);
        assert!(!inspect(br#"{"model":"m","stream":false}"#, 4096).stream);
    }

    #[test]
    fn unknown_fields_are_not_a_problem() {
        // the body belongs to the provider: a new field cannot make the request
        // unreadable for the gateway
        let body = br#"{"model":"m","temperature":0.7,"tools":[{"type":"function"}],"reasoning_effort":"high"}"#;
        assert_eq!(inspect(body, 4096).model.as_deref(), Some("m"));
    }

    #[test]
    fn an_unreadable_body_does_not_panic_and_is_an_unknown_model() {
        // it is not a gateway error: it is a request the provider will reject
        let shape = inspect("not json".as_bytes(), 4096);
        assert_eq!(shape.model, None);
        assert_eq!(shape.max_output_tokens, Some(4096));
        assert!(!shape.stream);
        assert_eq!(
            shape.estimated_input_tokens, 2,
            "8 bytes rounded up: 2 tokens"
        );
    }

    #[test]
    fn the_token_estimate_rounds_up() {
        assert_eq!(estimate_input_tokens(0), 0);
        assert_eq!(estimate_input_tokens(1), 1);
        assert_eq!(estimate_input_tokens(4), 1);
        assert_eq!(
            estimate_input_tokens(5),
            2,
            "never under: 5 bytes are at least 2 tokens"
        );
        assert_eq!(estimate_input_tokens(401), 101);
    }

    #[test]
    fn the_worst_case_uses_estimated_input_and_maximum_output() {
        let body = br#"{"model":"m","max_tokens":1000}"#;
        let shape = inspect(body, 4096);
        let price = crate::pricing::Price {
            input: 150,
            output: 600,
        };

        // the body is long: the input estimate must be part of the math
        let expected = price.cost(shape.estimated_input_tokens, 1000);
        assert_eq!(shape.worst_case_cost(price), Some(expected));
    }

    #[test]
    fn without_a_model_the_worst_case_is_not_computed() {
        let shape = inspect(b"broken", 4096);
        assert_eq!(shape.worst_case_cost(crate::pricing::Price::ZERO), None);
    }

    #[test]
    fn an_empty_body_is_not_a_panic() {
        let shape = inspect(b"", 4096);
        assert_eq!(shape.estimated_input_tokens, 0);
        assert_eq!(shape.body_bytes, 0);
    }
}
