//! The binary: a gateway you can actually run.
//!
//! Everything interesting is in the library. This file only does the wiring that a process
//! needs and a library must not do: read the configuration, read the secrets from the
//! environment, build the providers, bind a port, and stop cleanly.
//!
//! ```console
//! $ LLMGATEWAY_CONFIG=examples/gateway.toml cargo run --release
//! ```
//!
//! The configuration is validated **before** anything binds: a gateway that starts and then
//! serves with a broken price list is worse than a gateway that refuses to start.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llmgateway::auth::Authenticator;
use llmgateway::budget::{BudgetRegistry, Month, TenantBudget};
use llmgateway::config::{self, LoadedConfig};
use llmgateway::gateway::{Gateway, GatewayConfig};
use llmgateway::http::app;
use llmgateway::http_upstream::HttpUpstream;
use llmgateway::meter::Meter;
use llmgateway::request::DEFAULT_MAX_OUTPUT_TOKENS;
use llmgateway::router::Router;
use llmgateway::upstream::Upstream;
use llmgateway::usd;
use tokio::net::TcpListener;

const USAGE: &str = "\
llmgateway — an LLM gateway you can run

  llmgateway [--config path/to/gateway.toml]

Environment:
  LLMGATEWAY_CONFIG   configuration path (default: gateway.toml)
  LLMGATEWAY_LOG_JSON set to 1 for one JSON log line per record
  RUST_LOG            log filter (default: info)
";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return Ok(());
    }

    let path = config_path();
    let loaded = match config::from_path(&path) {
        Ok(loaded) => loaded,
        Err(errors) => {
            // every problem at once, and a non-zero exit: a half-configured gateway must not
            // start and then serve
            for error in &errors {
                eprintln!("config: {error}");
            }
            std::process::exit(2);
        }
    };

    init_tracing();
    let gateway = build(&loaded)?;
    let timeout = Duration::from_millis(loaded.config.server.request_timeout_ms);
    let listener = TcpListener::bind(&loaded.config.server.bind).await?;
    tracing::info!(
        bind = %listener.local_addr()?,
        providers = loaded.config.providers.len(),
        tenants = loaded.config.tenants.len(),
        models_priced = loaded.config.pricing.len(),
        timeout_ms = timeout.as_millis(),
        "llmgateway listening"
    );

    axum::serve(listener, app(gateway))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

fn config_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args
                .next()
                .map_or_else(|| PathBuf::from("gateway.toml"), PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            return PathBuf::from(value);
        }
    }
    std::env::var("LLMGATEWAY_CONFIG").map_or_else(|_| PathBuf::from("gateway.toml"), PathBuf::from)
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if std::env::var("LLMGATEWAY_LOG_JSON").is_ok_and(|value| value == "1") {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Builds the gateway from a validated configuration.
fn build(loaded: &LoadedConfig) -> Result<Arc<Gateway>, Box<dyn Error>> {
    let config = &loaded.config;
    let timeout = Duration::from_millis(config.server.request_timeout_ms);

    let mut upstreams: Vec<Arc<dyn Upstream>> = Vec::new();
    for provider in &config.providers {
        let key = loaded.provider_keys.get(&provider.name).ok_or_else(|| {
            format!(
                "provider {:?} has no key resolved from the environment",
                provider.name
            )
        })?;
        let models = if provider.models.is_empty() {
            None
        } else {
            Some(provider.models.iter().cloned().collect::<Vec<_>>())
        };
        upstreams.push(Arc::new(HttpUpstream::new(
            provider.name.clone(),
            provider.base_url.clone(),
            key.clone(),
            models,
        )));
    }

    let tenant_keys: Vec<(String, String)> = config
        .tenants
        .iter()
        .filter_map(|tenant| {
            loaded
                .keys
                .get(&tenant.id)
                .map(|key| (tenant.id.clone(), key.clone()))
        })
        .collect();
    let tenant_models: Vec<(String, BTreeSet<String>)> = config
        .tenants
        .iter()
        .map(|tenant| (tenant.id.clone(), tenant.models.clone()))
        .collect();

    let budgets = BudgetRegistry::new();
    let now = now_ms();
    for tenant in &config.tenants {
        let limit = usd(tenant.monthly_budget_usd)
            .ok_or_else(|| format!("tenant {:?} has an invalid monthly budget", tenant.id))?;
        budgets.insert(
            TenantBudget::new(tenant.id.clone(), limit, Month::of(now)),
            now,
        );
    }

    Ok(Arc::new(Gateway::new(GatewayConfig {
        authenticator: Authenticator::new(&tenant_keys, &tenant_models),
        budget: budgets,
        meter: Arc::new(Meter::new(
            config.pricing.clone(),
            config.server.metrics_sample_rate,
        )),
        router: Arc::new(Router::new(
            upstreams,
            config.server.max_attempts as usize,
            timeout,
        )),
        prices: config.pricing.clone(),
        max_output_default: DEFAULT_MAX_OUTPUT_TOKENS,
        timeout,
        now: Arc::new(now_ms),
    })))
}

fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    i64::try_from(millis).unwrap_or(i64::MAX)
}

async fn shutdown() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("shutting down"),
        Err(error) => tracing::error!(%error, "cannot listen for a shutdown signal"),
    }
}
