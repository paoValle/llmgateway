//! `llmgateway` — un proxy che mette tra te e i provider LLM quattro cose che il
//! provider non fa: sapere quanto costa, non superare il tetto, non cadere quando un
//! provider cade, e dirti quando è caduto.
//!
//! Lo stato è in memoria e il progetto è pensato per **una sola istanza**: vedi
//! [`docs/adr/0006-stato-in-memoria.md`](../docs/adr/0006-stato-in-memoria.md).

pub mod budget;
/// Quanto costa: micro-dollari interi, prezzi per modello.
pub mod config;
pub mod meter;
pub mod pricing;
pub mod request;
pub mod router;
pub mod upstream;

pub use pricing::{micros, usd, MicroUsd, Price, PriceTable, Usage};
