# ADR 0003 — What is the provider's is retried, what is the client's never is

- **Status:** accepted
- **Date:** 2026-10-04
- **Decides:** Paolo Valletta

## Context

In a gateway with failover, the question is always the same: when a provider is not doing
well, do we move to the next one? The easy answer — "always" — is wrong in half the cases,
and the mistake costs twice: money spent and user time.

A `400 Bad Request` is the **client's** fault: it sent a request no provider will accept.
Retrying on the next provider produces the same `400`, after a latency the user has already
seen. Worse: in a gateway that logs, one ends up attributing to a provider an error that is
the client's.

A `429 Too Many Requests` or a `503 Service Unavailable` is different: it is the provider's.
Moving to the next one is exactly why gateways exist.

An in-between case is `401`/`403`: invalid key or missing permission. Retrying with a
different provider might work (the error is on the key at *that* provider), but that is a
diagnosis the gateway cannot make: it could be a key expired everywhere. It retries only if
an error is **the provider's and about that provider**.

## Decision

A response is classified into three, and the classification decides:

| Class | Examples | Action |
|---|---|---|
| `Retryable` | 408, 429, 5xx, transport error, timeout | move to the next provider |
| `Client fault` | 400, 401, 403, 404, 413, 422 | return to the client, **no failover** |
| `Unknown` | any other status | **no failover**, alert log |

The `Unknown` case is the choice that must be defended in review: the reflex is to retry
everything else. But a `402 Payment Required` or a `451` have semantics the gateway does not
know, and on an error that was not understood the right thing is to **do nothing** and make
it noticeable. Failover is a convenience; a failover that amplifies an unknown error is a
disservice.

On top of that, two rules that hold things together:

- **no non-idempotent retry**: if the request was accepted and it is not clear whether it was
  executed (timeout after sending), we do not move to the next provider with the same
  request. A double charge is worse than an error;
- **attempt budget**: at most `len(providers)` attempts, and the sum of the retries across
  all providers is bounded by a configuration constant. Failover without a cap is a
  self-feeding DDoS.

## Alternatives

| Option | Pros | Cons | Why not |
|---|---|---|---|
| retry everything | simpler code | turns a user's 400 into 3 seconds of waiting and 3 log lines | it amplifies the error instead of propagating it |
| retry only 5xx | easy to remember | 429 is the most common case of all and would stay uncovered | it excludes exactly the case failover exists to cover |
| retry on the HTTP library default | no decision to make | the library knows nothing about tenants, providers and money | the decision is economic and domain: this is where it belongs |

## Consequences

**We win:**
- the logs make sense: an error is attributed to someone;
- the user's response time does not grow because of an error of theirs;
- double charges are excluded by construction.

**We pay:**
- a provider that answers `400` because of its own problem (not the client's) is not retried,
  and the client receives an error that was not theirs. We have no way to tell them apart. It
  is the accepted limit of this choice.

## Verification

One test per class, with a fake upstream that answers differently to different requests. If
the three classes are not all covered, the table is only documentation.
