//! Money: integer micro-dollars, prices per model, the cost of a request.
//!
//! The same rules as `agentloop`, for the same reason: `0.1 + 0.2 !== 0.3`, and on an
//! invoice the difference is a hole. No `f64` crosses this module.
//!
//! There is no reservation here: that lives in [`crate::budget`], which is where the
//! tenants are. This module only knows how to turn tokens into money.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An amount in micro-dollars. Integer, never `f64`.
///
/// `1 USD = 1 000 000 µUSD`. The granularity of a micro-dollar is far finer than that
/// of any price list, and a cap expressed in integers cannot accumulate rounding error
/// over time.
pub type MicroUsd = u64;

/// One dollar, in micro-dollars.
#[must_use]
pub fn usd(amount: f64) -> Option<MicroUsd> {
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    // `round` instead of truncation: 0.9999999999999999 would become 0, and a price
    // that becomes zero through rounding is a price that does not exist.
    #[allow(clippy::cast_sign_loss)] // the sign is already excluded by the checks above
    Some((amount * 1_000_000.0).round() as MicroUsd)
}

/// An amount already expressed in micro-dollars.
///
/// It is the Rust sibling of [`usd`]: needed where the value is already known and does
/// not come from a hand-written price list, and where passing it through `f64` would
/// only add an error.
#[must_use]
pub const fn micros(amount: u64) -> MicroUsd {
    amount
}

/// Prices of a model, in micro-dollars per **million** tokens.
///
/// A convenient unit because it is the one providers publish price lists in: it does
/// not matter whether a token costs 0.15 `µUSD` or 15 `µUSD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    /// Per million input tokens.
    pub input: MicroUsd,
    /// Per million output tokens.
    pub output: MicroUsd,
}

impl Price {
    /// A price of zero. Only used for "unknown price, do not count".
    pub const ZERO: Self = Self {
        input: 0,
        output: 0,
    };

    /// What a request consuming these tokens costs.
    ///
    /// Integer arithmetic, rounding **up**. A one micro-dollar error in the system's
    /// favor costs less than an invoice nobody can explain: the sum of many roundings
    /// down, at high volume, is the difference between a forecast and a surprise.
    ///
    /// The arithmetic in here saturates, it does not panic: with maximum prices and
    /// counts, `tokens × price` overflows `u64`, and a gateway that panics while
    /// computing a bill must not happen.
    #[must_use]
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> MicroUsd {
        let input = u128::from(input_tokens).saturating_mul(u128::from(self.input));
        let output = u128::from(output_tokens).saturating_mul(u128::from(self.output));
        // sum in u128: with u64 the product tokens × price overflows on an expensive
        // model and a long response
        div_ceil(input.saturating_add(output), 1_000_000)
    }

    /// The maximum price of the two, component by component.
    ///
    /// Used for the fallback on unknown models: valuing them at the highest known price
    /// means "we assume the worst", which is the only sensible assumption for a
    /// spending cap.
    #[must_use]
    pub fn max_componentwise(self, other: Self) -> Self {
        Self {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
        }
    }
}

/// Integer division rounded up. `div_ceil` has been stable since 1.73, but the project
/// declares `rust-version = 1.80`: it is written here so as not to depend on a version
/// detail in a place where the arithmetic is the point.
fn div_ceil(numerator: u128, denominator: u128) -> MicroUsd {
    numerator.div_ceil(denominator).min(u128::from(u64::MAX)) as MicroUsd
}

/// The prices per model.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    prices: BTreeMap<String, Price>,
    /// The highest known price: the fallback for models that are not there.
    worst: Option<Price>,
}

impl PriceTable {
    /// Builds from a model → price map.
    #[must_use]
    pub fn new(prices: BTreeMap<String, Price>) -> Self {
        let worst = prices.values().copied().reduce(Price::max_componentwise);
        Self { prices, worst }
    }

    /// Empty: every unknown model costs zero, and every cost is zero.
    ///
    /// It exists for tests and for a gateway with no price lists. **In production an
    /// empty table is a configuration error**, and `Config::validate` reports it.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The declared price of a model, if there is one.
    #[must_use]
    pub fn get(&self, model: &str) -> Option<Price> {
        self.prices.get(model).copied()
    }

    /// The price to use for a model: declared, or the worst known one.
    ///
    /// The pessimistic fallback is deliberate: a new model entering production is
    /// exactly the moment when a zero price would make the budget look like it protects
    /// something while it protects nothing.
    #[must_use]
    pub fn resolve(&self, model: &str) -> Price {
        self.get(model).or(self.worst).unwrap_or(Price::ZERO)
    }

    /// `true` if the model has a declared price.
    ///
    /// Used by logging to distinguish "real price" from "pessimistic estimate".
    #[must_use]
    pub fn knows(&self, model: &str) -> bool {
        self.prices.contains_key(model)
    }

    /// How many models have a declared price.
    #[must_use]
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// `true` if no model has a price.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }
}

/// Tokens consumed by a request, as reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens (prompt).
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens (completion).
    #[serde(default)]
    pub output_tokens: u64,
}

impl Usage {
    /// Cost of these tokens at the given price.
    #[must_use]
    pub fn cost(&self, price: Price) -> MicroUsd {
        price.cost(self.input_tokens, self.output_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> PriceTable {
        let mut m = BTreeMap::new();
        m.insert(
            "cheap".to_owned(),
            Price {
                input: 150,
                output: 600,
            },
        );
        m.insert(
            "expensive".to_owned(),
            Price {
                input: 2_500,
                output: 10_000,
            },
        );
        PriceTable::new(m)
    }

    #[test]
    fn one_dollar_is_a_million_micro_dollars() {
        assert_eq!(usd(1.0), Some(1_000_000));
        assert_eq!(usd(0.25), Some(250_000));
        assert_eq!(usd(0.0), Some(0));
    }

    #[test]
    fn micro_dollars_are_integers_and_do_not_pass_through_floats() {
        // from here on there is no f64 left in the project that touches a price
        assert_eq!(micros(150), 150);
        assert_eq!(micros(1_000_000), usd(1.0).expect("1 USD"));
    }

    #[test]
    fn a_negative_or_non_finite_price_is_not_a_price() {
        assert_eq!(usd(-1.0), None);
        assert_eq!(usd(f64::NAN), None);
        assert_eq!(usd(f64::INFINITY), None);
    }

    #[test]
    fn the_cost_is_computed_on_the_price_list_per_million() {
        let p = Price {
            input: 3,
            output: 15,
        };
        // 1M + 1M tokens = 3 + 15 µUSD
        assert_eq!(p.cost(1_000_000, 1_000_000), 18);
        assert_eq!(p.cost(0, 0), 0);
    }

    #[test]
    fn the_cost_rounds_up_never_down() {
        let p = Price {
            input: 3,
            output: 15,
        };
        // 1 token at 3 µUSD/M = 0.000003 µUSD: it must be worth 1, not 0
        assert_eq!(p.cost(1, 0), 1);
        assert_eq!(p.cost(0, 1), 1);
        assert_eq!(p.cost(1, 1), 1);
    }

    /// A realistic price: 3 `µUSD` per million tokens is a price list that does not exist.
    const PRICE: Price = Price {
        input: 150,
        output: 600,
    };
    const REQUESTS: u64 = 1_000;
    const TOKENS_PER_REQUEST: u64 = 1_000;

    #[test]
    fn summing_many_requests_is_never_under_the_real_cost() {
        let p = PRICE;
        let counted: MicroUsd = (0..REQUESTS).map(|_| p.cost(TOKENS_PER_REQUEST, 500)).sum();
        // the real cost, rounded only once over everything
        let real = p.cost(TOKENS_PER_REQUEST * REQUESTS, 500 * REQUESTS);

        assert!(
            counted >= real,
            "never under: {counted} < {real} — the cap no longer protects"
        );
        // and by how much it is wrong: at most one micro-dollar per request, for rounding
        let excess = counted - real;
        assert!(
            excess <= REQUESTS,
            "overestimate of {excess} over {REQUESTS} requests: more than 1 µUSD per request"
        );
    }

    #[test]
    fn an_overflowing_product_does_not_panic() {
        // tokens × price does not fit in u64, and neither does the sum of the two in
        // u128: the result must be a number, not a panic
        let p = Price {
            input: u64::MAX,
            output: u64::MAX,
        };
        let cost = p.cost(u64::MAX, u64::MAX);
        assert_eq!(cost, u64::MAX);
    }

    #[test]
    fn max_componentwise_keeps_the_worst_side() {
        let a = Price {
            input: 100,
            output: 5_000,
        };
        let b = Price {
            input: 900,
            output: 1,
        };
        let m = a.max_componentwise(b);
        assert_eq!(
            m,
            Price {
                input: 900,
                output: 5_000
            }
        );
    }

    #[test]
    fn a_known_model_has_its_own_price() {
        assert_eq!(table().resolve("expensive").input, 2_500);
        assert!(table().knows("expensive"));
    }

    #[test]
    fn an_unknown_model_is_worth_the_highest_price_we_know() {
        let price = table().resolve("model-of-the-future");
        // not zero: a zero price means "the budget protects" and that is not true
        assert_eq!(price.input, 2_500);
        assert_eq!(price.output, 10_000);
        assert!(!table().knows("model-of-the-future"));
    }

    #[test]
    fn an_empty_table_gives_zero_and_says_so() {
        assert!(PriceTable::empty().is_empty());
        assert_eq!(PriceTable::empty().resolve("any"), Price::ZERO);
    }

    #[test]
    fn usage_deserializes_from_the_provider_format() {
        // the field is called prompt_tokens upstream: renaming it is the adapter's job
        let json = r#"{"input_tokens":120,"output_tokens":40}"#;
        let u: Usage = serde_json::from_str(json).expect("valid usage");
        assert_eq!(
            u,
            Usage {
                input_tokens: 120,
                output_tokens: 40
            }
        );
    }
}
