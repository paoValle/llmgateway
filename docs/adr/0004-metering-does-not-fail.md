# ADR 0004 — Metering can never lose a response

- **Status:** accepted
- **Date:** 2026-10-04
- **Decides:** Paolo Valletta

## Context

The gateway has two jobs with the same response in front of it: **deliver it to the client**
and **record what it cost**. They are in tension: the second is local and in memory, the
first is what the user is waiting for.

The natural way to write the code is to do things in sequence: fetch the response, update the
counters, then forward. It works as long as updating the counters is fast, or does not fail,
or the process is not under memory pressure.

At that point the question becomes: **in whose favor is the response sacrificed?** And the
obvious answer, "in the counter's favor", is the wrong one. If the user does not receive the
response, the agent does not work, and the ticket that arrives is "the gateway is slow" — not
"we lost a counter". A lost counter is recovered by looking at the provider's invoice. A lost
response is not.

There is also the real-error case: a bug in metering — an `unwrap` on something unexpected, an
overflow, a missing key — that becomes a 500 for a user who did everything right.

## Decision

**The accountant does not bring the service down.**

1. **Response first, accounting after.** The upstream response is forwarded to the client
   before metering is updated. If metering fails, the response has already gone out.
2. **Metering has no way to fail.** There is no `Result`: `record()` has a signature that
   admits no failures. If something is off (unknown model, overflowing counter), the function
   **degrades and records it**, with a `metering_errors_total` counter going up.
3. **The fallback is pessimistic.** If the model is not in the table, it is estimated with the
   highest known price, as in `agentloop`: a bill that is wrong in excess is recoverable, a
   bill that is wrong in defect is a hole nobody notices until the invoice.
4. **Counting does not block.** An `RwLock` over a global counter would become a bottleneck
   under load. The counters are per tenant in a map behind a short `RwLock`, and the metrics
   are **approximated by sampling** (ADR 0005): counting every request costs more than the
   datum is worth.

## Alternatives

| Option | Pros | Cons | Why not |
|---|---|---|---|
| count exactly and blocking | the number is true | an error in the counter kills the response; under load it is a bottleneck | it optimizes the metric at the expense of the service |
| count on an async channel (mpsc) | the response never waits | if the channel is full the messages are lost, and there is no way to know | it moves the loss, it does not avoid it |
| if metering fails, answer 500 | the failure is visible | an internal bug becomes a disservice for the user | the user can do nothing about a counting bug |
| save to disk on every request | survives restarts | I/O per request on a critical path | persistence is outside the perimeter (RFC), and that must be said |

## Consequences

**We win:**
- a counting error is a counter and a log line, not a disservice;
- the response time does not depend on writing the counters.

**We lose:**
- **the count can lose requests.** That is the declared price, and its consistency is
  approximate by sampling (ADR 0005). It must be said in the README, because a user who
  invoices on these numbers must know: the authoritative datum remains the provider's.
- if the gateway crashes, the current window's accounting dies with it.

## Verification

A test that makes metering fail and verifies the client still receives `200` with the right
body. If that test does not exist, this ADR is an intention.
