# Changelog

Format [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning [SemVer](https://semver.org/).

## [Unreleased]

### Added

- A model priced by fallback is named in a `warn` line the first time it is served, with the
  price being used. The `llmgateway_estimated` counter is exact but silent: it only reaches
  whoever reads the dashboard, and the first person to learn that a new model is in production
  was usually the invoice.

## [0.2.0] - 2026-10-05

The library becomes something you can run: the two transport adapters and a binary.

### Added
- `http` module: the HTTP surface (axum) — `POST /v1/chat/completions` with byte-for-byte
  forwarding, `/metrics`, streaming pass-through, no body-size limit, and
  `serve_ephemeral` so tools can bind a port chosen by the operating system.
- `http_upstream` module: an `Upstream` that really speaks HTTP (reqwest), with the error
  classification that decides whether a failed request may be retried at all.
- A binary (`src/main.rs`): configuration from a file, secrets from the environment,
  structured logging, graceful shutdown.
- 9 integration tests that run a real HTTP provider on an ephemeral port, including a provider
  that hangs after receiving the request and must therefore not be retried.

### Fixed
- `TenantSnapshot` carried no tenant id, so `/metrics` could not label anything.
- A comment on the missing-model branch claimed the gateway does not judge the body while the
  code answers `400` itself without calling a provider. The behaviour is right (no model means
  no price and no routing); the comment now says that.

## [0.1.0] - 2026-10-04

First usable version: metering, per-tenant budget, failover, metrics — as a library.
