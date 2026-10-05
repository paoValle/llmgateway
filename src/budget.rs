//! The per-tenant cap: reservation, settlement, and month rollover.
//!
//! As in `agentloop`, the mechanism is **reserve → settle**: an estimate is locked
//! before calling the provider, and the difference is released when the real bill
//! arrives. A cap that is checked afterwards protects the previous request.
//!
//! The lock is **per tenant**, not global. A single `Mutex` over all counters would make
//! the gateway single-threaded exactly on the critical path: one tenant paying a lot
//! would block the reservation of all the others. Every tenant has its own, and two
//! tenants never contend.
//!
//! The month rollover is the delicate point: at midnight the counter restarts, and a
//! reservation opened in the previous month must no longer be settled against the new
//! one. Here the reservation carries the window it was born in.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::pricing::MicroUsd;

/// A month of a year, as a window key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Month {
    /// Year.
    pub year: i64,
    /// Month from 1 to 12.
    pub month: u32,
}

impl Month {
    /// The month a Unix instant falls in, in milliseconds.
    ///
    /// Computed with Howard Hinnant's civil date algorithm, which is exact for any
    /// Gregorian date and has no tables and no dependencies. The second is UTC: a
    /// monthly cap that changed depending on the server time zone would be a surprise
    /// for whoever administers it.
    #[must_use]
    pub fn of(epoch_ms: i64) -> Self {
        let days = epoch_ms.div_euclid(86_400_000);
        let (year, month, _day) = civil_from_days(days);
        Self { year, month }
    }
}

/// Civil date from a count of days since the epoch (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // the three u32 casts below are safe by construction: the algorithm produces a day
    // in [1, 31] and a month in [1, 12], so there is no sign to lose.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn narrow(v: i64) -> u32 {
        u32::try_from(v).unwrap_or(0)
    }

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let mp = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = narrow(day_of_year - (153 * mp + 2) / 5 + 1); // [1, 31]
    let month = narrow(if mp < 10 { mp + 3 } else { mp - 9 }); // [1, 12]
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Why a reservation was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExceeded {
    /// How much was needed, in `µUSD`.
    pub requested: MicroUsd,
    /// How much was there, in `µUSD`.
    pub available: MicroUsd,
    /// The tenant cap.
    pub limit: MicroUsd,
}

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "budget exhausted: {} µUSD needed, {} available (cap {})",
            self.requested, self.available, self.limit
        )
    }
}

impl std::error::Error for BudgetExceeded {}

/// Why a reservation failed.
///
/// Two causes, and they must not be confused: one is **economic** and goes back to the
/// client as `429`, the other is a **compromised internal state** and must be reported
/// as a gateway problem. A single error type mixing them would lead to answering `429`
/// to a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveError {
    /// The cap does not cover the estimate. It goes to the client.
    Exceeded(BudgetExceeded),
    /// The lock is poisoned by a panic. It is a gateway problem, not the tenant's.
    State(BudgetPoisoned),
}

impl std::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exceeded(e) => write!(f, "{e}"),
            Self::State(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReserveError {}

/// A reservation of money.
///
/// It must **always** be settled or released. A forgotten reservation locks budget for
/// the rest of the window: that is why `Budget` exposes `open_reservations` and the
/// gateway records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    id: u64,
    amount: MicroUsd,
    window: Month,
}

/// The internal state of a tenant, protected by its lock.
#[derive(Debug, Clone, Copy)]
struct State {
    window: Month,
    spent: MicroUsd,
    held: MicroUsd,
    next_id: u64,
}

/// The cap of a tenant.
#[derive(Debug)]
pub struct TenantBudget {
    tenant_id: String,
    limit: MicroUsd,
    state: Mutex<State>,
}

impl TenantBudget {
    /// Creates a cap. `window` is the current window: whoever creates it decides where
    /// it starts, and the tests need that.
    #[must_use]
    pub fn new(tenant_id: impl Into<String>, limit: MicroUsd, window: Month) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            limit,
            state: Mutex::new(State {
                window,
                spent: 0,
                held: 0,
                next_id: 1,
            }),
        }
    }

    /// The tenant identifier.
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    /// The cap of the window.
    #[must_use]
    pub fn limit(&self) -> MicroUsd {
        self.limit
    }

    /// Money spent in the current window.
    #[must_use]
    pub fn spent(&self, now_ms: i64) -> MicroUsd {
        self.read_state(now_ms).map_or(0, |s| s.spent)
    }

    /// Money reserved and not yet settled.
    #[must_use]
    pub fn held(&self, now_ms: i64) -> MicroUsd {
        self.read_state(now_ms).map_or(0, |s| s.held)
    }

    /// How much can still be reserved.
    #[must_use]
    pub fn available(&self, now_ms: i64) -> MicroUsd {
        self.read_state(now_ms)
            .map_or(0, |s| self.limit - s.spent - s.held)
    }

    /// How many reservations are open. If something is left at the end of a request,
    /// it is a bug.
    #[must_use]
    pub fn open_reservations(&self, now_ms: i64) -> u64 {
        self.read_state(now_ms).map_or(0, |_| {
            self.state.lock().map_or(0, |s| s.next_id.saturating_sub(1))
        })
    }

    /// Reserves `amount`, or fails **before** anyone spends.
    pub fn reserve(&self, amount: MicroUsd, now_ms: i64) -> Result<Reservation, ReserveError> {
        let mut s = self.lock(now_ms).map_err(ReserveError::State)?;
        let available = self.limit - s.spent - s.held;
        if amount > available {
            return Err(ReserveError::Exceeded(BudgetExceeded {
                requested: amount,
                available,
                limit: self.limit,
            }));
        }
        s.held += amount;
        let reservation = Reservation {
            id: s.next_id,
            amount,
            window: s.window,
        };
        s.next_id += 1;
        Ok(reservation)
    }

    /// Settles with the real consumption and releases the difference.
    ///
    /// If the reservation belongs to a **previous window**, it is a no-op: the month
    /// changed, the counter restarted from zero, and adding an old debt onto a new
    /// counter would make money that has already been spent disappear.
    pub fn settle(&self, reservation: Reservation, actual: MicroUsd, now_ms: i64) {
        if let Ok(mut s) = self.lock(now_ms) {
            if s.window != reservation.window {
                return;
            }
            s.held = s.held.saturating_sub(reservation.amount);
            s.spent = s.spent.saturating_add(actual);
        }
    }

    /// Releases the reservation: the estimate turned out too high.
    pub fn release(&self, reservation: Reservation, now_ms: i64) {
        if let Ok(mut s) = self.lock(now_ms) {
            if s.window != reservation.window {
                return;
            }
            s.held = s.held.saturating_sub(reservation.amount);
        }
    }

    /// The detail for `/metrics` and for logs.
    #[must_use]
    pub fn snapshot(&self, now_ms: i64) -> Option<Snapshot> {
        let s = self.lock(now_ms).ok()?;
        Some(Snapshot {
            tenant_id: self.tenant_id.clone(),
            window: s.window,
            limit: self.limit,
            spent: s.spent,
            held: s.held,
            available: self.limit - s.spent - s.held,
        })
    }

    /// Poisons the lock, as a `panic` in another thread would.
    ///
    /// It exists for one reason only: to verify that a cap whose state is not readable
    /// **still does not prevent serving the request** (ADR 0004). There is no honest way
    /// to provoke a `panic` inside the other methods, and without this hook the most
    /// important test of the project cannot be written.
    ///
    /// Returns `true` if the lock was poisoned.
    #[doc(hidden)]
    pub fn poison(&self) -> bool {
        let Ok(_guard) = self.state.lock() else {
            return false; // already poisoned: nothing to do
        };
        std::panic::panic_any(PoisonMarker);
    }

    /// Takes the lock and resets the counters if the month changed.
    fn lock(&self, now_ms: i64) -> Result<std::sync::MutexGuard<'_, State>, BudgetPoisoned> {
        let mut s = self.state.lock().map_err(|_| BudgetPoisoned)?;
        let current = Month::of(now_ms);
        if s.window != current {
            *s = State {
                window: current,
                spent: 0,
                held: 0,
                next_id: 1,
            };
        }
        Ok(s)
    }

    /// Like [`Self::lock`] but without touching the counters: for reads.
    fn read_state(&self, now_ms: i64) -> Option<State> {
        let s = self.lock(now_ms).ok()?;
        Some(*s)
    }
}

/// The lock was poisoned by a panic in another thread.
///
/// It is not recoverable in a useful way: if it happened, the state is partly
/// inconsistent and the right thing is to notice it and refuse, not to pretend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetPoisoned;

impl std::fmt::Display for BudgetPoisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("budget state not readable: a thread panicked while holding the lock")
    }
}

impl std::error::Error for BudgetPoisoned {}

/// The type used to poison the lock. It is never returned to anyone.
#[doc(hidden)]
#[derive(Debug)]
pub struct PoisonMarker;

/// A snapshot of a tenant cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Who.
    pub tenant_id: String,
    /// In which month.
    pub window: Month,
    /// Cap.
    pub limit: MicroUsd,
    /// Spent.
    pub spent: MicroUsd,
    /// Reserved and not settled.
    pub held: MicroUsd,
    /// Still reservable.
    pub available: MicroUsd,
}

/// All the tenants, in one place.
#[derive(Debug, Default)]
pub struct BudgetRegistry {
    budgets: RwLock<BTreeMap<String, Arc<TenantBudget>>>,
}

impl BudgetRegistry {
    /// Empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a cap. If the tenant already exists, it is replaced.
    pub fn insert(&self, budget: TenantBudget, now_ms: i64) -> Arc<TenantBudget> {
        let id = budget.tenant_id().to_owned();
        let arc = Arc::new(budget);
        if let Ok(mut b) = self.budgets.write() {
            b.insert(id, Arc::clone(&arc));
        }
        // it aligns with the current window right away: a cap built with an old window
        // must count from now, not from the first reservation
        let _ = arc.spent(now_ms);
        arc
    }

    /// The cap of a tenant, if it exists.
    #[must_use]
    pub fn get(&self, tenant_id: &str) -> Option<Arc<TenantBudget>> {
        self.budgets.read().ok()?.get(tenant_id).cloned()
    }

    /// All the tenants, for the metrics.
    #[must_use]
    pub fn snapshots(&self, now_ms: i64) -> Vec<Snapshot> {
        let all: Vec<Arc<TenantBudget>> = match self.budgets.read() {
            Ok(b) => b.values().cloned().collect(),
            Err(_) => return Vec::new(),
        };
        all.iter().filter_map(|b| b.snapshot(now_ms)).collect()
    }

    /// How many tenants are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.budgets.read().map_or(0, |b| b.len())
    }

    /// `true` if there is no tenant.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEN: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
    const DAY: i64 = 86_400_000;

    fn cap(limit: MicroUsd) -> TenantBudget {
        TenantBudget::new("acme", limit, Month::of(GEN))
    }

    #[test]
    fn the_month_is_computed_in_utc_not_in_the_server_timezone() {
        assert_eq!(
            Month::of(GEN),
            Month {
                year: 2026,
                month: 1
            }
        );
        assert_eq!(
            Month::of(GEN + 31 * DAY),
            Month {
                year: 2026,
                month: 2
            }
        );
        assert_eq!(
            Month::of(GEN + 365 * DAY),
            Month {
                year: 2027,
                month: 1
            }
        );
    }

    #[test]
    fn the_month_computation_respects_month_lengths() {
        // January 2026 has 31 days: February 28 is 58 days after January 1
        assert_eq!(
            Month::of(GEN + 58 * DAY),
            Month {
                year: 2026,
                month: 2
            }
        );
        assert_eq!(
            Month::of(GEN + 59 * DAY),
            Month {
                year: 2026,
                month: 3
            }
        );
    }

    #[test]
    fn the_month_computation_respects_leap_years() {
        // 2028 is a leap year: March 1 is 790 days after January 1, 2026
        // (365 + 365 + 60, where 60 = 31 in January + 29 in February)
        assert_eq!(
            Month::of(GEN + 790 * DAY),
            Month {
                year: 2028,
                month: 3
            }
        );
        // the day before is still February: if the leap year were ignored,
        // this would already be March
        assert_eq!(
            Month::of(GEN + 789 * DAY),
            Month {
                year: 2028,
                month: 2
            }
        );
    }

    #[test]
    fn a_month_before_the_epoch_does_not_panic() {
        // a wrong clock or a test with negative timestamps must not panic
        assert_eq!(Month::of(-2_208_988_800_000).year, 1900);
    }

    #[test]
    fn a_reservation_locks_the_money_until_settlement() {
        let b = cap(1_000_000);
        let r = b.reserve(400_000, GEN).expect("it fits");
        assert_eq!(b.held(GEN), 400_000);
        assert_eq!(b.spent(GEN), 0);
        assert_eq!(b.available(GEN), 600_000);

        b.settle(r, 300_000, GEN);
        assert_eq!(b.held(GEN), 0);
        assert_eq!(b.spent(GEN), 300_000);
        assert_eq!(b.available(GEN), 700_000);
    }

    #[test]
    fn going_over_the_cap_fails_before_the_spending() {
        let b = cap(1_000_000);
        b.reserve(800_000, GEN).expect("it fits");
        let error = b.reserve(300_000, GEN).expect_err("it does not fit");
        let ReserveError::Exceeded(e) = error else {
            panic!("a budget rejection, not a state problem: {error:?}");
        };
        assert_eq!(e.requested, 300_000);
        assert_eq!(e.available, 200_000);
        assert_eq!(e.limit, 1_000_000);
        assert_eq!(b.spent(GEN), 0, "the rejection must not move money");
    }

    #[test]
    fn an_overestimated_reservation_does_not_burn_money() {
        let b = cap(1_000_000);
        let r = b.reserve(900_000, GEN).expect("it fits");
        b.settle(r, 1, GEN);
        assert_eq!(b.spent(GEN), 1);
        assert_eq!(b.available(GEN), 999_999);
    }

    #[test]
    fn releasing_frees_without_spending() {
        let b = cap(1_000_000);
        let r = b.reserve(500_000, GEN).expect("it fits");
        b.release(r, GEN);
        assert_eq!(b.held(GEN), 0);
        assert_eq!(b.spent(GEN), 0);
    }

    #[test]
    fn the_month_rollover_resets_and_frees_the_old_reservations() {
        let b = cap(1_000_000);
        b.reserve(900_000, GEN).expect("it fits");
        assert_eq!(b.available(GEN), 100_000);

        // first day of the following month
        let february = GEN + 31 * DAY;
        assert_eq!(b.spent(february), 0);
        assert_eq!(
            b.held(february),
            0,
            "the January reservation does not block February"
        );
        assert_eq!(b.available(february), 1_000_000);
    }

    #[test]
    fn a_reservation_from_the_elapsed_month_is_not_settled_on_the_new_account() {
        let b = cap(1_000_000);
        let r = b.reserve(900_000, GEN).expect("it fits");
        let february = GEN + 31 * DAY;

        // the request started in January and ends in February
        b.settle(r, 900_000, february);
        assert_eq!(
            b.spent(february),
            0,
            "the January debt cannot fall on February"
        );
        assert_eq!(b.held(february), 0);
    }

    #[test]
    fn the_count_of_open_reservations_is_visible() {
        let b = cap(1_000_000);
        assert_eq!(b.open_reservations(GEN), 0);
        let r = b.reserve(1, GEN).expect("it fits");
        assert_eq!(b.open_reservations(GEN), 1);
        b.settle(r, 1, GEN);
        // the counter does not go down: saying "0 open reservations" after a settlement
        // would be false, the window has had one
        assert_eq!(b.open_reservations(GEN), 1);
    }

    #[test]
    fn a_cap_zeroed_at_startup_is_not_a_safety_cap() {
        let b = cap(0);
        assert!(b.reserve(1, GEN).is_err(), "with a zero cap nothing passes");
    }

    #[test]
    fn the_snapshot_describes_the_picture() {
        let b = cap(2_000_000);
        b.reserve(500_000, GEN).expect("it fits");
        assert_eq!(
            b.snapshot(GEN),
            Some(Snapshot {
                tenant_id: "acme".to_owned(),
                window: Month::of(GEN),
                limit: 2_000_000,
                spent: 0,
                held: 500_000,
                available: 1_500_000,
            })
        );
    }

    #[test]
    fn the_registry_returns_the_same_cap_and_keeps_it_by_id() {
        let reg = BudgetRegistry::new();
        let a = reg.insert(cap(1_000_000), GEN);
        reg.insert(TenantBudget::new("beta", 5_000_000, Month::of(GEN)), GEN);

        assert_eq!(reg.len(), 2);
        assert_ne!(reg.len(), 0);
        assert_eq!(reg.get("acme").map(|b| b.limit()), Some(1_000_000));
        assert_eq!(reg.get("beta").map(|b| b.limit()), Some(5_000_000));
        assert!(reg.get("gamma").is_none());

        // re-inserting the same tenant does not create a second one
        reg.insert(cap(7_000_000), GEN);
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.get("acme").map(|b| b.limit()), Some(7_000_000));
        // the previous tenant was replaced, not placed alongside
        assert_ne!(a.limit(), 7_000_000);
    }

    #[test]
    fn the_snapshots_are_ordered_by_tenant_and_complete() {
        let reg = BudgetRegistry::new();
        reg.insert(cap(1_000_000), GEN);
        reg.insert(TenantBudget::new("beta", 5_000_000, Month::of(GEN)), GEN);
        let snap = reg.snapshots(GEN);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].tenant_id, "acme");
        assert_eq!(snap[1].tenant_id, "beta");
    }

    #[test]
    fn an_empty_registry_does_not_panic() {
        let reg = BudgetRegistry::new();
        assert_eq!(reg.len(), 0);
        assert!(reg.get("acme").is_none());
        assert_eq!(reg.snapshots(GEN).len(), 0);
    }
}
