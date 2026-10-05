# Changelog

Format [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning [SemVer](https://semver.org/).

## [Unreleased]

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
