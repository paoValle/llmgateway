//! The client side: the HTTP surface of the gateway.
//!
//! `Gateway::handle` is a pure function from a `GatewayRequest` to a `GatewayResponse`, and
//! this is the few lines that turn it into a server. Three decisions live here and nowhere
//! else:
//!
//! - **the body is read as bytes**, with the body-size limit disabled: a gateway that
//!   truncates a prompt at 2 MB is a gateway that corrupts requests, and the limit that
//!   matters is the provider's, not ours;
//! - **a stream is a stream**: a `GatewayResponse::Stream` becomes a streaming HTTP body, so
//!   the client receives the first token when it arrives;
//! - **`/metrics` says what it measures**: money is exact, and the counters that are sampled
//!   carry `sampled_` in their name (ADR 0005).

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;

use crate::gateway::{Gateway, GatewayRequest, GatewayResponse};

/// The HTTP surface of a gateway.
pub fn app(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(completions))
        .route("/metrics", get(metrics))
        .layer(DefaultBodyLimit::disable())
        .with_state(gateway)
}

/// Serves until the listener closes. The caller owns the listener, so it can choose the port.
pub async fn serve(listener: TcpListener, gateway: Arc<Gateway>) -> std::io::Result<()> {
    axum::serve(listener, app(gateway)).await
}

/// Binds an ephemeral port and serves in the background.
///
/// Exists for tests and for tools like `llmlab`: an HTTP surface that cannot be started on a
/// port chosen by the operating system is an HTTP surface nobody can measure.
pub async fn serve_ephemeral(
    gateway: Arc<Gateway>,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app(gateway)).await {
            tracing::error!(%error, "the HTTP server stopped");
        }
    });
    Ok((address, handle))
}

async fn completions(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = GatewayRequest {
        key: bearer(&headers),
        body: body.to_vec(),
    };

    match gateway.handle(request).await {
        GatewayResponse::Whole {
            status,
            body,
            provider,
        } => {
            let mut response = (status_of(status), body).into_response();
            if let Some(provider) = provider {
                if let Ok(value) = header::HeaderValue::from_str(&provider) {
                    response
                        .headers_mut()
                        .insert("x-llmgateway-provider", value);
                }
            }
            response
        }
        GatewayResponse::Stream {
            status,
            chunks,
            provider,
        } => {
            let mut response = Response::new(Body::from_stream(chunks));
            *response.status_mut() = status_of(status);
            if let Ok(value) = header::HeaderValue::from_str(&provider) {
                response
                    .headers_mut()
                    .insert("x-llmgateway-provider", value);
            }
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/event-stream"),
            );
            response
        }
    }
}

/// The Prometheus text exposition, with no dependency: it is a loop and a `writeln!`.
///
/// The names carry the precision: money is exact, and every counter that is sampled says so
/// (ADR 0005). A dashboard built on this file must be able to tell the two apart without
/// reading the code.
async fn metrics(State(gateway): State<Arc<Gateway>>) -> Response {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let now_ms = i64::try_from(now_ms).unwrap_or(i64::MAX);

    let mut out = String::from(
        "# money: exact, never sampled
# TYPE llmgateway_spend_micro_usd gauge
",
    );
    let mut sampled = String::from(
        "# operating counters: sampled, hence the name (ADR 0005)\n\
         # TYPE llmgateway_sampled_requests_total counter\n\
         # TYPE llmgateway_sampled_budget_denied_total counter\n",
    );
    let mut alarms = String::from(
        "# TYPE llmgateway_served_without_cap_total counter\n\
         # on zero the cap protects everyone; on anything else, someone spent unchecked\n",
    );

    for tenant in gateway.meter().snapshots() {
        let id = escape_label(&tenant.tenant_id);
        // writing into a String cannot fail, and a metrics endpoint that panics on its own
        // exposition is worse than one that prints less
        let _ = writeln!(
            out,
            "llmgateway_spend_micro_usd{{tenant=\"{id}\"}} {}",
            tenant.spent
        );
        let _ = writeln!(
            sampled,
            "llmgateway_sampled_requests_total{{tenant=\"{id}\"}} {}",
            tenant.served
        );
        let _ = writeln!(
            sampled,
            "llmgateway_sampled_budget_denied_total{{tenant=\"{id}\"}} {}",
            tenant.budget_denied
        );
        let _ = writeln!(
            alarms,
            "llmgateway_served_without_cap_total{{tenant=\"{id}\"}} {}",
            tenant.uncovered
        );
    }

    for snapshot in gateway.budgets().snapshots(now_ms) {
        let id = escape_label(&snapshot.tenant_id);
        let _ = writeln!(
            out,
            "llmgateway_budget_micro_usd{{tenant=\"{id}\",kind=\"limit\"}} {}",
            snapshot.limit
        );
        let _ = writeln!(
            out,
            "llmgateway_budget_micro_usd{{tenant=\"{id}\",kind=\"held\"}} {}",
            snapshot.held
        );
    }

    let body = format!("{out}{sampled}{alarms}");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ").unwrap_or(value);
    Some(token.trim().to_owned())
}

fn status_of(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

/// Prometheus label values cannot contain an unescaped quote or backslash.
fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
