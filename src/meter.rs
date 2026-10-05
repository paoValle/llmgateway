//! Metering: what it cost, and to whom.
//!
//! A module with a single property, which is why it exists as a separate module:
//! **it has no way to fail.** No `Result`, no panic, no dependency that can be missing.
//! If something is off — a model not in the price list, a counter that overflows, a
//! tenant never seen before — the function degrades, and the fact that it degraded is
//! visible in [`Meter::errors`].
//!
//! The reason is in ADR 0004 and is worth repeating: if the accountant brings down the
//! service, the user does not get the response and the ticket that arrives is "the
//! gateway is slow". A lost counter is recovered from the provider's invoice. A lost
//! response is not.
//!
//! Two precisions, and the difference is intended (ADR 0005):
//!
//! - **money is exact**, per tenant. It is not sampled: it is the number the gateway
//!   exists for.
//! - **operating metrics are sampled** 1 in N. Counting them all costs a lock on every
//!   request, and the global counter becomes a bottleneck.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::pricing::{MicroUsd, Price, PriceTable, Usage};
use crate::request::RequestShape;

/// A tenant, with its exact counters.
#[derive(Debug, Default)]
struct TenantTotals {
    /// Money spent. **Never sampled**: it is the number the gateway exists for.
    spent: AtomicU64,
    /// Requests served successfully.
    served: AtomicU64,
    /// Requests rejected by the cap.
    budget_denied: AtomicU64,
    /// Requests that did not reach any provider.
    failed: AtomicU64,
    /// Requests served **without the cap**, because its state was not readable.
    ///
    /// It is the most important alarm of the gateway: from that moment the cap no longer
    /// protects anyone, and as long as the number is zero the service is under control.
    uncovered: AtomicU64,
}

/// How a request ended, from the accounting point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A 2xx response from a provider.
    Served {
        /// Who answered.
        provider: String,
        /// Whether the price list entry used is an estimate. If the model was not in the
        /// table, the worst known price was used, not zero.
        price_estimated: bool,
    },
    /// Rejected by the cap, without any provider being called.
    BudgetDenied,
    /// No response arrived from any provider.
    Failed {
        /// The last provider tried, for the log.
        provider: String,
    },
}

/// The counter.
#[derive(Debug)]
pub struct Meter {
    prices: PriceTable,
    /// The tenants encountered. They grow with usage, not with configuration: that is
    /// the difference between a gateway that knows its clients and one that discovers
    /// them.
    totals: RwLock<BTreeMap<String, Arc<TenantTotals>>>,
    /// Sampling rate for operating metrics. 1 = exact.
    sample_rate: u64,
    /// Sampling ticker: every N events, one is counted.
    ticker: AtomicU64,
    /// Times metering had to degrade. It goes up instead of failing.
    errors: AtomicU64,
    /// Requests seen in total, even when not sampled.
    seen: AtomicU64,
    /// Bills computed on a model outside the price list: they must be checked by hand.
    estimated: AtomicU64,
    /// Served without the cap, across all tenants.
    uncovered_total: AtomicU64,
}

impl Meter {
    /// Creates a counter. `sample_rate` is 1 in N for operating metrics.
    #[must_use]
    pub fn new(prices: PriceTable, sample_rate: u64) -> Self {
        Self {
            prices,
            totals: RwLock::new(BTreeMap::new()),
            sample_rate: sample_rate.max(1),
            ticker: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            estimated: AtomicU64::new(0),
            uncovered_total: AtomicU64::new(0),
        }
    }

    /// The price that will be used for a model, and whether it is the declared one.
    ///
    /// `price_estimated = true` means the model was not in the price list and the worst
    /// known price is being used: it is an alarm, not a detail.
    #[must_use]
    pub fn price_for(&self, model: &str) -> (Price, bool) {
        (self.prices.resolve(model), !self.prices.knows(model))
    }

    /// Records a served request. **It cannot fail.**
    pub fn record(&self, tenant: &str, shape: &RequestShape, usage: Usage, outcome: &Outcome) {
        self.seen.fetch_add(1, Ordering::Relaxed);

        // the only place where metering can degrade, and it declares that it did
        let (price, estimated) = if let Some(model) = shape.model.as_deref() {
            self.price_for(model)
        } else {
            // no model: no price list entry, no cost. The error is counted and we move
            // on — the provider will answer that it does not understand the request
            self.errors.fetch_add(1, Ordering::Relaxed);
            (Price::ZERO, true)
        };
        if estimated {
            // a model outside the price list is an alarm, not a detail: the cap is
            // computed on the worst price and the bill must be checked
            self.estimated.fetch_add(1, Ordering::Relaxed);
        }

        let cost = price.cost(usage.input_tokens, usage.output_tokens);

        // money is exact, and it is not sampled: it is the number the gateway exists for
        self.totals_for(tenant)
            .spent
            .fetch_add(cost, Ordering::Relaxed);

        if !self.should_sample() {
            return;
        }
        let counters = self.totals_for(tenant);
        match outcome {
            Outcome::Served { .. } => {
                counters.served.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::BudgetDenied => {
                counters.budget_denied.fetch_add(1, Ordering::Relaxed);
            }
            &Outcome::Failed { .. } => {
                counters.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Records a request served **without the cap having been checked**.
    ///
    /// It only happens when the cap state is not readable. It is not an error to hide:
    /// it is the moment the cap stops protecting, and the number goes on a dashboard for
    /// the alarm.
    pub fn record_uncovered(&self, tenant: &str) {
        self.uncovered_total.fetch_add(1, Ordering::Relaxed);
        self.totals_for(tenant)
            .uncovered
            .fetch_add(1, Ordering::Relaxed);
    }

    /// How many requests in total were served without the cap.
    ///
    /// At zero, the cap protects. At any other number, it does not: and until it goes
    /// back to zero the gateway is spending without control.
    #[must_use]
    pub fn served_uncovered(&self) -> u64 {
        self.uncovered_total.load(Ordering::Relaxed)
    }

    /// Records a rejection by the cap. The provider was not called: the money spent is
    /// zero, but the request is information that is needed anyway.
    pub fn record_budget_denied(&self, tenant: &str) {
        self.seen.fetch_add(1, Ordering::Relaxed);
        let counters = self.totals_for(tenant);
        if self.should_sample() {
            counters.budget_denied.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How much a tenant spent. **Exact**, never sampled.
    ///
    /// A tenant never seen before spent zero, not "I do not know": the gateway knows it
    /// from the configuration, and if there is no recorded error it did not spend.
    #[must_use]
    pub fn spent(&self, tenant: &str) -> MicroUsd {
        self.totals_for(tenant).spent.load(Ordering::Relaxed)
    }

    /// The snapshot of a tenant, if it exists.
    ///
    /// `None` means that tenant has not made any request yet: not that it spent zero.
    /// The difference matters in `/metrics`, where an absent row and a zero tell
    /// different stories.
    #[must_use]
    pub fn snapshot_for(&self, tenant: &str) -> Option<TenantSnapshot> {
        let t = self.totals.read().ok()?.get(tenant)?.clone();
        Some(snapshot_of(&t))
    }

    /// How many requests of a tenant were denied by the cap.
    ///
    /// Sampled like the other operating metrics: it is a diagnostic number, not a bill.
    #[must_use]
    pub fn denied(&self, tenant: &str) -> u64 {
        self.snapshot_for(tenant).map_or(0, |s| s.budget_denied)
    }

    /// The snapshots of all tenants encountered, for `/metrics`.
    #[must_use]
    pub fn snapshots(&self) -> Vec<TenantSnapshot> {
        let all: Vec<Arc<TenantTotals>> = match self.totals.read() {
            Ok(t) => t.values().cloned().collect(),
            Err(_) => return Vec::new(),
        };
        all.iter().map(|t| snapshot_of(t)).collect()
    }

    /// How many times metering had to degrade. At zero, everything went fine.
    #[must_use]
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Bills computed on a model **outside the price list**, with the worst known price.
    ///
    /// At zero, every model that went through is a model that is known. At a high
    /// number, either the price list needs updating, or the gateway is serving something
    /// that was not expected.
    #[must_use]
    pub fn estimated(&self) -> u64 {
        self.estimated.load(Ordering::Relaxed)
    }

    /// How many requests went through, sampled or not.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen.load(Ordering::Relaxed)
    }

    /// The effective sampling rate: `1` means every metric is exact.
    #[must_use]
    pub fn sample_rate(&self) -> u64 {
        self.sample_rate
    }

    /// The totals of a tenant, creating them if they do not exist.
    ///
    /// If the lock is poisoned an error is recorded and a throwaway counter is returned:
    /// **the request is served anyway**, which is the point.
    fn totals_for(&self, tenant: &str) -> Arc<TenantTotals> {
        if let Ok(t) = self.totals.read() {
            if let Some(existing) = t.get(tenant) {
                return Arc::clone(existing);
            }
        }

        let fresh = Arc::new(TenantTotals::default());
        if let Ok(mut t) = self.totals.write() {
            return Arc::clone(
                t.entry(tenant.to_owned())
                    .or_insert_with(|| Arc::clone(&fresh)),
            );
        }
        // the lock was poisoned by someone else's panic: we degrade, count the error, and
        // the request goes on with a throwaway counter
        self.errors.fetch_add(1, Ordering::Relaxed);
        fresh
    }

    /// Ticks the sampling counter and says whether this event should be counted.
    fn should_sample(&self) -> bool {
        let n = self.ticker.fetch_add(1, Ordering::Relaxed) + 1;
        n % self.sample_rate == 0
    }
}

fn snapshot_of(t: &TenantTotals) -> TenantSnapshot {
    TenantSnapshot {
        spent: t.spent.load(Ordering::Relaxed),
        served: t.served.load(Ordering::Relaxed),
        budget_denied: t.budget_denied.load(Ordering::Relaxed),
        failed: t.failed.load(Ordering::Relaxed),
        uncovered: t.uncovered.load(Ordering::Relaxed),
    }
}

/// A tenant snapshot for `/metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantSnapshot {
    /// Money spent, **exact**.
    pub spent: MicroUsd,
    /// Requests served, sampled.
    pub served: u64,
    /// Rejections by the cap, sampled.
    pub budget_denied: u64,
    /// Failed, sampled.
    pub failed: u64,
    /// Served without the cap. **Never sampled**: it is an alarm, not a statistic.
    pub uncovered: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::micros;

    fn prices() -> PriceTable {
        let mut m = BTreeMap::new();
        m.insert(
            "cheap".to_owned(),
            Price {
                input: micros(150),
                output: micros(600),
            },
        );
        m.insert(
            "expensive".to_owned(),
            Price {
                input: micros(2_500),
                output: micros(10_000),
            },
        );
        PriceTable::new(m)
    }

    fn shape(model: &str) -> RequestShape {
        RequestShape {
            model: Some(model.to_owned()),
            max_output_tokens: Some(1_000),
            stream: false,
            estimated_input_tokens: 1_000,
            body_bytes: 4_000,
        }
    }

    fn meter() -> Meter {
        Meter::new(prices(), 1)
    }

    #[test]
    fn a_tenant_bill_is_exact() {
        let m = meter();
        m.record(
            "acme",
            &shape("cheap"),
            Usage {
                input_tokens: 1_000,
                output_tokens: 500,
            },
            &Outcome::Served {
                provider: "a".to_owned(),
                price_estimated: false,
            },
        );

        // 1000×150/1e6 + 500×600/1e6 = 0.15 + 0.3 = 0.45 µUSD → 1 rounded up
        assert_eq!(m.spent("acme"), 1);
    }

    #[test]
    fn the_bills_of_two_tenants_do_not_mix() {
        let m = meter();
        m.record(
            "acme",
            &shape("expensive"),
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 0,
            },
            &Outcome::Served {
                provider: "a".to_owned(),
                price_estimated: false,
            },
        );
        m.record(
            "beta",
            &shape("cheap"),
            Usage {
                input_tokens: 1,
                output_tokens: 0,
            },
            &Outcome::Served {
                provider: "a".to_owned(),
                price_estimated: false,
            },
        );

        assert_eq!(m.spent("acme"), 2_500);
        assert_eq!(m.spent("beta"), 1);
        assert_eq!(m.spent("gamma"), 0, "a tenant never seen before spent zero");
    }

    #[test]
    fn an_unknown_model_is_counted_at_the_worst_price_and_says_so() {
        let m = meter();
        let (price, estimated) = m.price_for("model-of-the-future");
        assert!(estimated, "the gateway must know it is estimating");
        assert_eq!(
            price,
            Price {
                input: micros(2_500),
                output: micros(10_000)
            }
        );
        assert!(!m.price_for("cheap").1);
    }

    #[test]
    fn a_shape_with_no_model_does_not_panic_and_accounts_for_zero() {
        let m = meter();
        let mut f = shape("cheap");
        f.model = None;
        m.record(
            "acme",
            &f,
            Usage {
                input_tokens: 1_000,
                output_tokens: 500,
            },
            &Outcome::Served {
                provider: "a".to_owned(),
                price_estimated: false,
            },
        );
        // without a model there is no price: zero is counted and the error is recorded,
        // but the request was served anyway
        assert_eq!(m.spent("acme"), 0);
        assert_eq!(m.errors(), 1);
    }

    #[test]
    fn sampling_saves_the_metrics_but_not_the_money() {
        // this is the distinction of ADR 0005: counting every request on a global counter
        // costs a lock on every request, and money must stay exact
        let m = Meter::new(prices(), 100);
        for _ in 0..1_000 {
            m.record(
                "acme",
                &shape("expensive"),
                Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 0,
                },
                &Outcome::Served {
                    provider: "a".to_owned(),
                    price_estimated: false,
                },
            );
        }

        let snap = m.snapshots();
        assert_eq!(snap.len(), 1);
        // money is exact: 1000 requests at 2 500 µUSD each
        assert_eq!(snap[0].spent, 2_500 * 1_000);
        // operating metrics are estimated: 1000 events at rate 100 → 10
        assert_eq!(snap[0].served, 10);
        assert_eq!(m.seen(), 1_000, "the requests seen are all counted");
    }

    #[test]
    fn a_sampling_rate_of_one_means_exact() {
        let m = Meter::new(prices(), 1);
        for _ in 0..50 {
            m.record(
                "acme",
                &shape("cheap"),
                Usage::default(),
                &Outcome::Served {
                    provider: "a".to_owned(),
                    price_estimated: false,
                },
            );
        }
        assert_eq!(m.snapshots()[0].served, 50);
        assert_eq!(m.sample_rate(), 1);
    }

    #[test]
    fn a_rate_of_zero_means_count_everything_not_nothing() {
        assert_eq!(Meter::new(prices(), 0).sample_rate(), 1);
    }

    #[test]
    fn a_request_served_without_the_cap_is_the_main_alarm() {
        let m = meter();
        assert_eq!(m.served_uncovered(), 0, "at zero the cap protects");

        m.record_uncovered("acme");
        m.record_uncovered("beta");

        assert_eq!(m.served_uncovered(), 2);
        assert_eq!(m.snapshot_for("acme").map(|s| s.uncovered), Some(1));
    }

    #[test]
    fn rejections_by_the_cap_are_counted_but_do_not_spend() {
        let m = meter();
        m.record_budget_denied("acme");
        m.record_budget_denied("acme");
        let snap = m.snapshots();
        assert_eq!(snap[0].budget_denied, 2);
        assert_eq!(
            snap[0].spent, 0,
            "a rejection costs nothing: no provider was called"
        );
    }

    #[test]
    fn failures_are_counted_and_spend_zero() {
        let m = meter();
        m.record(
            "acme",
            &shape("expensive"),
            Usage::default(),
            &Outcome::Failed {
                provider: "a".to_owned(),
            },
        );
        let snap = m.snapshots();
        assert_eq!(snap[0].failed, 1);
        assert_eq!(snap[0].spent, 0);
    }

    #[test]
    fn the_counts_appear_in_the_snapshots_after_the_first_use() {
        let m = meter();
        assert_eq!(m.snapshots().len(), 0);
        m.record_budget_denied("new");
        assert_eq!(m.snapshots().len(), 1);
    }
}
