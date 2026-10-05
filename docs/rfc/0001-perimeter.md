# RFC 0001 — The perimeter of `llmgateway`

> Before writing a line of code: what is being discussed, what is not, and why.
> Status: **accepted**. The technical decisions that follow from it live in `../adr/`.

## The problem

An application that calls an LLM provider in production has three needs the provider does
not cover, and they all become urgent together with the first paying client.

**1. What it costs.** Price lists are per million tokens, per model, with different prices
for input and output. The only datum the API returns is the usage **of the last request**.
From there to "how much has this tenant spent to date" there is a spreadsheet someone has
to keep updated, and that gets it wrong.

**2. Not exceeding the budget.** A runaway in an agent — a loop that calls the provider too
many times, a retry that feeds itself, a re-entry bug — can burn a day of billing before
anyone notices. A cap that is checked **after** the call protects the previous request, not
the one that is spending.

**3. Not falling over when a provider falls over.** A 429 or a 503 from a provider is a
fact that happened, not an exception: it must be handled like a normal working day, with
another provider taking over and without the end user noticing.

The common point of the three is that they are **crossings**: a tenant, a moment in time,
a model. It is exactly the work no library does well and every team rewrites.

## What it does (v0.1)

- **Reverse proxy** for `POST /v1/chat/completions`, OpenAI-compatible.
- **Tenant** identified by API key: every request is attributed to someone.
- **Per-tenant budget** (monthly, in integer micro-dollars) checked **before** the call,
  with a reservation as in `agentloop`.
- **Metering**: the real consumption of the response, attributed to tenant and model.
- **Failover** over an ordered list of providers, only for retryable errors.
- **Streaming** in pass-through: the SSE chunks reach the client when they arrive, with no
  buffering.
- **Prometheus metrics** on `/metrics`, text, with no dependencies.
- **Structured JSON logs** on one line.

## What it does NOT do (v0.1)

| It does not do | Why not |
|---|---|
| Prompt caching | useful, but it is another policy (ttl, key, invalidation) and it comes after the rest is green |
| Per-RPS rate limiting | the per-tenant budget is the limit that is needed; rate limiting is a second check with a second semantics |
| Non-OpenAI protocols | Anthropic has different shapes for streaming and usage: they are a per-provider adapter, not a change to the gateway |
| Authentication beyond static keys | OAuth, mTLS, rotation: it is an identity problem, not a gateway one |
| Persistence | the state is in memory. On multiple instances every instance has its own count: it must be said (ADR 0006), not hidden |
| Hot configuration reload | you restart it. A reload in the middle of traffic is a consistency case not worth a version |

## The success contract

- [ ] a tenant over budget receives `429` **without the provider being called**
- [ ] a `429` or a `503` from the first provider ends up at the second, and the client does not notice
- [ ] a `400` from the provider is **not** retried: it is the client's fault, not the provider's
- [ ] streaming is not buffered: the first chunk goes out before the upstream has finished
- [ ] the metrics count requests, errors and cost per tenant and per model
- [ ] no test touches the network: the suite runs in under 30 seconds
- [ ] `cargo clippy -- -D warnings` green, `cargo fmt` clean

## Rejected alternatives

- **Using a managed gateway** (Helicone, Portkey, LiteLLM hosted). The exact need, zero
  control. And the point of this project is to demonstrate being able to build it, not
  being able to buy it.
- **Doing it in Go**. The better language would have been Go: network, processes, no
  contortions. Rust was chosen for a precise and declared reason — the code of a proxy is
  full of `Result`, shared state and conversions between formats, and that is where the
  borrow checker forces choices that in Go would be made at random. If the project turns
  out to be simple network plumbing, the choice must be revised: it is a goal.
- **Extending `agentloop` with a proxy module**. agentloop is a library and must stay one.
  A gateway is a process, it has a server, it has state. Separating is right.

## Open questions

- **How is a tenant identified?** I chose a static key in `Authorization: Bearer`, mapped
  to a tenant in the configuration. It is fine as long as the tenants are few and known.
  With self-managed tenants something else is needed: that is discussed when it is needed.
- **How big is the reservation?** The cost of the request is not known before making it. I
  use a per-step estimate (estimated input + maximum output from the body), which is
  pessimistic but exhaustive: see ADR 0002.
- **Does metering block the response?** No: it is local and in memory, but an error in
  metering must never lose a response that is already ready. See ADR 0004.
