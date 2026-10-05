# ADR 0001 — Dependencies: a small, declared set, not zero

- **Status:** accepted
- **Date:** 2026-10-04
- **Decides:** Paolo Valletta

## Context

`agentloop` has zero runtime dependencies, and that was possible because it is a library:
its only contract is types and functions, and every line of parsing can be written by hand
in an afternoon.

A gateway is different: it is a process that listens, accepts connections, makes HTTP calls
to third parties and has to handle streaming. Rebuilding this without libraries means:

- a `TcpListener` with hand-written HTTP parsing, chunked encoding and keep-alive included;
- a connection pool with timeouts and retries, written from scratch;
- an asynchronous runtime, because a slow call must not occupy a thread;
- an SSE parser;
- a TOML parser.

None of these is an interesting problem: they are solved problems, with many known pitfalls,
by people who spent years on them. The value of the project is in the **budget, the failover
and the metering**, not in having rewritten HTTP.

## Decision

A minimal set of dependencies, all from the "official" Rust ecosystem:

| Crate | Why this one and not another |
|---|---|
| `tokio` | the de facto asynchronous runtime; `axum` does not work without it |
| `axum` | routing and extractors on `tower`, and it is the standard HTTP server interface |
| `reqwest` | HTTP client with streaming support and connection pooling |
| `serde` + `serde_json` | serialization; it is not negotiable in a project like this |
| `toml` | declared and validated configuration at startup |
| `thiserror` | typed errors, one per line, without macros expanding in debug |
| `tracing` + `tracing-subscriber` | structured logging to sinks; a `println!` is not a log |

Nothing else. In particular:

- **no `serde_yaml`**: TOML takes the format question off the table and is better suited to
  a configuration with a repeatable `[[provider]]` section;
- **no Prometheus crate**: the exposition is text and can be written in thirty lines; adding
  `prometheus` means adding per-process metrics, collection queues and a transitive
  dependency, for a formatter;
- **no configuration framework**: validation is a function returning a list of readable
  errors, and it is more useful than a framework.

## Alternatives

| Option | Pros | Cons | Why not |
|---|---|---|---|
| std + own implementation | zero CVEs, zero supply chain | 1500 lines of HTTP/async before writing one euro of logic | it is a different, and less interesting, project |
| `hyper` instead of `axum` | one level less | routing and extractors to write by hand | `axum` is thin over `hyper`: it does not add a level, it hides it |
| `prometheus` crate | ready-made metrics | collection, queues, feature flags for a text formatter | overkill for the text of `/metrics` |
| Go | simpler network and concurrency | — | a deliberate choice: see the RFC. It is the only thing I decided not to do, and it must be revised if the project simplifies |

## Consequences

**We win:**
- reproducible build (committed `Cargo.lock`) and a small, inspectable supply chain surface;
- every line I could have written in thirty minutes was spent on the budget and the failover.

**We pay:**
- periodic security updates. With `cargo audit`/`dependabot` they are a habit, not an event;
- the initial compile time is high, and on Windows it links lazily. Neither is a project problem.

## Verification

If the graph of direct dependencies exceeds the seven rows of the table above, or if
`cargo tree` shows that one direct dependency brings three, one of which is not needed, the
decision must be revised: it means the library is carrying our responsibilities inside it.
