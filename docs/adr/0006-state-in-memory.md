# ADR 0006 — The state lives in memory, and a single instance is the only way to believe it

- **Status:** accepted
- **Date:** 2026-10-04
- **Decides:** Paolo Valletta

## Context

The counters of a gateway are by definition per tenant and per time window. The tenant is
known from the API key; the window is known from the clock. Both things lead to state that
has to exist somewhere.

The three options are: in memory, on disk, in a database.

The database is the right answer in general and the wrong answer here, for a precise reason:
**the number of requests the gateway handles is already the number of requests the provider
is handling**. Nobody puts a Postgres in front of an API that does not ask for it. But the
point is another one: if the state is on disk or in a database, the gateway has acquired a
way of losing data (disk full, connection dropped) that it did not have.

In memory, the loss is only in the case of a crash, and it is **limited to the last window**:
on restart you start from zero for the tenant, and the monthly cap restarts with it. That is
not a detail, it is a hole in the guarantee.

## Decision

State in memory, in coverage broader than the budget of ADR 0001 would like, and **the single
instance is a declared requirement**, not a caveat.

- `v0.1` is designed and documented for **a single instance**. With two, every instance has its
  own count and a tenant moving from one to the other sees its cap halved.
- No distributed lock is added to remedy that: it would cost a dependency and a latency on
  every request to cover a case that does not exist in the intended configuration.
- If more instances are needed, the way is a counter backend (Redis, or the provider itself as
  the authoritative source) and **one more piece for the cap**: a distributed reservation is
  needed, and ADR 0002 turns it into a protocol, not a line.

Concretely, the README says:

> **A single instance.** With more than one process, every instance counts for itself and the
> per-tenant cap is no longer a cap. This project is a side project: declaring the limit is
> worth more than putting on top of it a distributed lock you would not know how to run.

And the code makes it visible: the state exposes the number of requests served, so an operator
who sees the cap skipping knows they have more instances before suspecting a bug.

## Alternatives

| Option | Pros | Cons | Why not |
|---|---|---|---|
| in memory only | simple, fast, no dependency | the count dies on restart | it is declared, and the hole is limited to the current window |
| on disk (append file) | survives restarts | I/O and fsync on the critical path, or a background writer that is a second queue | it complicates failover of the critical path |
| SQLite | simple, survives | locking writes, the same problems as disk | same, with a format pretending to be a database |
| Redis | multi-instance, fast | operational dependency, and the reservation becomes distributed | the right answer to the wrong problem for this version |

## Consequences

**We win:**
- no operational dependency beyond the binary;
- the critical path touches no disk and no network beyond the upstream.

**We lose:**
- on restart the counters start from zero;
- **more instances break the cap.** It is the most important limit of the project and it is
  written in the README, not hidden in a detail.

## Verification

If the project were to take on real users, the first change to make is a counter backend. The
test that makes it necessary: two instances, one tenant, one cap — and the cap that does not
fire. That test is not there now, because a single instance is a requirement and not a bug.
