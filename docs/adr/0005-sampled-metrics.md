# ADR 0005 — The metrics are sampled, and the README says so

- **Status:** accepted
- **Date:** 2026-10-04
- **Decides:** Paolo Valletta

## Context

A Prometheus gateway is measured in three ways: a counter incremented on every request, a
timer recording every duration, or both. The obvious shape is to count everything.

The cost of counting everything is not the counter: it is the **contention**. Every request
that increments a global counter takes a lock. With a single lock for the process, the
gateway becomes single-threaded on a path that by definition runs in parallel. Under real
load, the contention is visible in latency.

So the question is not "are metrics useful" — they are — but "how much exactness is needed,
and who needs it".

## Decision

**Sampling with a global atomic counter**, with a known and declared error.

A global `AtomicU64`, with no lock, with a counter advancing on every event. The per-tenant
and per-model metrics are updated only when sampling says "this is one of the N". No
`Mutex<HashMap<>>` on the hot path.

What is lost, explicitly:

- **the per-tenant metrics are estimates**, not exact: with N = 100, every tenant is counted
  with an error on the order of ±√N/100 on the sample. That is fine for a dashboard, not fine
  for an invoice;
- **the duration metrics are sampled**, not an exact histogram.

What is **not** sampled, because it needs to be exact: **money**. The per-tenant total is
always exact, because it is an `AtomicU64` per tenant, not a table. The sampling is for the
operating metrics (`requests_total`, errors, durations), not for `spend_micro_usd`.

And the thing that matters most, written in the README:

> **the authoritative datum for cost is the provider's invoice.** This gateway counts to give
> you continuous control and an early alarm, not to replace the invoice.

A dashboard that looks authoritative and is not is worse than no dashboard: it makes people
take decisions on wrong numbers.

## Alternatives

| Option | Pros | Cons | Why not |
|---|---|---|---|
| count everything with `Mutex<HashMap>` | exact and simple | contention on every request; the gateway becomes single-threaded | the cost is on the critical path |
| count everything with per-tenant atomics | exact, low contention | still a per-tenant map: unbounded growth and a lookup per request | the number of tenants is unbounded, the number of entries is not |
| histogram only, no counters | low cost | counters are the first metric people look at | it gives up half the datum to avoid paying for the other half |
| exact counters in a separate process | exact, no contention on the hot path | another process to manage, and the datum is asynchronous anyway | complexity for a datum that does not need to be exact |

## Consequences

**We win:**
- the hot path takes no lock;
- the per-tenant cost stays exact, which is the number the gateway exists for.

**We pay:**
- the operating metrics are estimates, and it must be written somewhere that they are.
  "Somewhere" means **in the README and in the metric name**: `sampled_`, not
  `requests_total` which sounds exact.

## Verification

The test must show that the sampled count stays within the expected error under a known load,
and that `spend_micro_usd` is exact. If the former oscillates too much, `N` is too high.
