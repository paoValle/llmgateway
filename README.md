# llmgateway

> LLM gateway: per-token metering, per-tenant budget, failover between providers, Prometheus metrics.

**Status:** v0.2 — a library **and** a server you can run. No SLA, no users beyond me and my
notes. It is designed for **a single instance**, and that is a limit, not an oversight: with
two processes each one counts for itself and the per-tenant cap stops being a cap
([ADR 0006](docs/adr/0006-state-in-memory.md)).

## The problem

An application that calls an LLM provider in production has three needs the provider does not
cover, and they all become urgent together with the first paying client:

1. **What it costs.** Price lists are per million tokens, per model. The API returns the usage
   of *the last request*; from there to "what has this tenant spent today" there is a
   spreadsheet somebody keeps by hand, and that gets it wrong.
2. **Not exceeding the budget.** A runaway agent can burn a day of billing before anyone
   notices. A cap checked **after** the call protects the previous request, not the one that
   is spending.
3. **Not falling over when a provider falls over.** A `429` or a `503` is a fact that happened,
   not an exception: another provider takes over and the client does not notice.

The common point is that all three are **crossings**: a tenant, a moment in time, a model — the
work no library does well and every team rewrites.

## What it does

- **Reverse proxy** for `POST /v1/chat/completions`, OpenAI-compatible, forwarding the body
  **byte for byte**: no re-serialization, no field lost because the gateway did not know it.
- **Tenant** identified by API key; only the SHA-256 hash is kept in memory, never the key.
- **Per-tenant monthly cap** checked **before** the call, by reservation; a rejected request
  never reaches a provider.
- **Exact metering** in integer micro-dollars, attributed to tenant and model.
- **Failover** over an ordered provider list, only for errors that are retryable — and never
  for a request that may already have been executed.
- **Streaming** in pass-through, chunk by chunk, with no buffering.
- **Prometheus metrics** on `/metrics`: money is exact, and every sampled counter says
  `sampled_` in its name ([ADR 0005](docs/adr/0005-sampled-metrics.md)).
- **Structured logs** on one line, one JSON object per record if you want it.

## What it does NOT do

| It does not do | Why not |
|---|---|
| Prompt caching | a second policy (ttl, key, invalidation) with its own failure modes |
| Rate limiting per RPS | the per-tenant budget is the limit that matters; RPS is a second semantics |
| Protocols other than OpenAI | Anthropic has different shapes for usage and streaming: a per-provider adapter, not a change here |
| Authentication beyond static keys | OAuth, mTLS, rotation: an identity problem, not a gateway one |
| Persistence | state in memory, on purpose, and the single instance is declared |
| Hot configuration reload | you restart it. A reload mid-traffic is a consistency case not worth a version |

The full denied perimeter is in [RFC 0001](docs/rfc/0001-perimeter.md).

## Usage

```bash
cp examples/gateway.toml gateway.toml          # no secrets in there: only variable names
export PRIMARY_API_KEY=sk-...                  # the provider key
export TENANT_ACME_KEY=a-long-random-string    # the tenant key your client will send

cargo run --release -- --config gateway.toml
```

```console
$ curl -s localhost:8080/v1/chat/completions \
    -H 'Authorization: Bearer a-long-random-string' \
    -d '{"model":"gpt-4o-mini","max_tokens":256,"messages":[{"role":"user","content":"hello"}]}'

$ curl -s localhost:8080/metrics
# money: exact, never sampled
# TYPE llmgateway_spend_micro_usd gauge
llmgateway_spend_micro_usd{tenant="acme"} 42
# operating counters: sampled, hence the name (ADR 0005)
# TYPE llmgateway_sampled_requests_total counter
llmgateway_sampled_requests_total{tenant="acme"} 7
```

Environment variables: `LLMGATEWAY_CONFIG` (default `gateway.toml`), `RUST_LOG`,
`LLMGATEWAY_LOG_JSON=1` for one JSON log line per record.

Make targets: `make setup`, `make test`, `make lint`, `make fmt`, `make ci`.

## Architecture

```
client ──► http::app            (axum: bytes in, bytes out, no body limit)
             │
             ├─► auth            who it is, and whether it may use this model
             ├─► budget          reserve an estimate  ◄── before the call, not after
             ├─► router ───────► http_upstream (reqwest) ──► provider
             │     failover: only errors that are the provider's,
             │     and never a request that may already have been executed
             └─► meter           settle the real usage, exact in micro-dollars
```

```text
src/
  http.rs           the client side: axum, /metrics, streaming pass-through
  http_upstream.rs  the provider side: reqwest, and the error classification that matters
  main.rs           the binary: configuration, secrets from the environment, shutdown
  gateway.rs        auth + cap + failover + accounting in one path, as a pure function
  router.rs         failover policy, and the three classes of error
  upstream.rs       what a provider is, and whether a request can be retried
  budget.rs         monthly per-tenant reservation, with the month rollover
  meter.rs          exact money, sampled operating counters
  pricing.rs        micro-dollars, integer arithmetic
  config.rs         TOML, validated all at once, secrets only by reference
  auth.rs           SHA-256 key hashes, per-tenant model allow-lists
  request.rs        the three fields the gateway reads out of a body
```

`Gateway::handle` is a pure function from a request to a response: the transport adapters
(`http.rs`, `http_upstream.rs`) are the only files that know about the network, which is why
the whole decision path is tested without a socket.

## Tests

126 tests, no network, in under a second: 84 unit tests, 14 for the router's failover, 19 for
the gateway path, and 9 integration tests that run a real HTTP provider on an ephemeral port —
byte-for-byte forwarding, unknown key, cap enforced before the provider is called, `503`
failover, a provider that hangs after receiving (not retried), streaming, `/metrics`.

```bash
make ci     # cargo fmt --check && cargo clippy -D warnings && cargo test --all-targets
```

The measured behaviour of this gateway — the numbers, not the claims — is in
[`llmlab`](https://github.com/paoValle/llmlab).

## Decisions

The non-obvious choices are in [`docs/adr/`](docs/adr/): dependencies, the cap before the call,
what is retryable, metering that cannot lose a response, sampled metrics, state in memory.

## What I would do differently

- **One instance is a real limitation.** Multi-instance needs a distributed reservation, which
  is a protocol, not a lock. It is declared instead of hidden, but it is the first thing a real
  deployment would need.
- **The price list is a hand-written file.** A model that appears in production without a price
  is valued at the highest known one: pessimistic and safe, but the moment a new model ships
  somebody should be told automatically, not by reading a dashboard.
- **Streaming is metered downstream.** The usage of a streamed response arrives in its last
  chunk, which is never buffered, so a streamed run is billed by whatever is downstream. It is
  declared in the RFC and in the report; it is still a hole.
- **A cap that is only monthly.** A tenant that spends its whole month in an hour is within the
  rules and probably should not be.

## Development

```bash
git clone git@github.com:paoValle/llmgateway.git
cd llmgateway
make setup && make ci
```

## License

MIT © Paolo Valletta
