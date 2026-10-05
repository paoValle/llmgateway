//! `llmgateway` — a proxy that puts four things between you and the LLM providers
//! that the provider does not do: knowing what things cost, not exceeding the cap,
//! not falling over when a provider falls over, and telling you when it did.
//!
//! The state is in memory and the project is meant for **a single instance**: see
//! [`docs/adr/0006-state-in-memory.md`](../docs/adr/0006-state-in-memory.md).

pub mod auth;
pub mod budget;
/// What things cost: integer micro-dollars, prices per model.
pub mod config;
pub mod gateway;
pub mod meter;
pub mod pricing;
pub mod request;
pub mod router;
pub mod upstream;

pub use pricing::{micros, usd, MicroUsd, Price, PriceTable, Usage};
