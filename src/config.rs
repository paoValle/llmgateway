//! Configuration: how a gateway is declared.
//!
//! One principle only, and it holds for the whole file: **no secrets in here.** Keys
//! are taken from the environment, and the configuration says *where* to look for them.
//! The file is committable, the diff is readable, and there is no `api_key = "sk-..."`
//! left in a repository by someone.
//!
//! Validation **collects all** errors before reporting them. A configuration that stops
//! at the first problem forces a round trip per error; one that lists them all does the
//! work once.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::Path;

use serde::Deserialize;

use crate::pricing::{Price, PriceTable};

/// The complete configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where it listens and how much it tolerates. By default: sensible values.
    #[serde(default)]
    pub server: ServerConfig,
    /// The providers, in order of preference.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// The prices per model, in dollars per million tokens.
    #[serde(default)]
    pub pricing: BTreeMap<String, PriceDollars>,
    /// The tenants that may use the gateway.
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
}

/// `[server]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Listen address.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Wait cap for one upstream call. 60 s by default: below that, a slow provider
    /// looks dead.
    #[serde(default = "default_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Cap on **total** attempts, across all providers.
    ///
    /// Failover without a cap is a self-feeding attack (`DDoS`): every provider that does
    /// not answer leads to trying the next one, and if the providers are all unresponsive
    /// it multiplies load exactly when load is already at the limit.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Operating metric sampling rate: 1 in N. 1 = exact.
    #[serde(default = "default_sample_rate")]
    pub metrics_sample_rate: u64,
}

impl Default for ServerConfig {
    /// Written by hand and not derived: the defaults here are choices, not neutral
    /// values, and `derive(Default)` would give zeros — a bind to `0.0.0.0:0` and a null
    /// timeout.
    fn default() -> Self {
        Self {
            bind: default_bind(),
            request_timeout_ms: default_timeout_ms(),
            max_attempts: default_max_attempts(),
            metrics_sample_rate: default_sample_rate(),
        }
    }
}

/// A `[[providers]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Name used in logs and metrics.
    pub name: String,
    /// Base URL of the API, without `/chat/completions`.
    pub base_url: String,
    /// Environment variable holding the key. **Not** the key.
    pub api_key_env: String,
    /// Order of preference: lower first.
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Models this provider serves. Empty = "all the ones the tenant declares".
    #[serde(default)]
    pub models: BTreeSet<String>,
}

/// A `[[tenants]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    /// Tenant identifier: it appears in metrics and logs.
    pub id: String,
    /// Environment variable holding this tenant's key.
    ///
    /// The gateway looks it up on arrival and compares its value. The key never travels
    /// through any log (see `redact.rs`).
    pub key_env: String,
    /// Monthly cap, in dollars.
    pub monthly_budget_usd: f64,
    /// Models this tenant may use. Empty = all.
    #[serde(default)]
    pub models: BTreeSet<String>,
}

/// Prices in the format you write by hand: dollars per million tokens.
///
/// `serde` converts it into integer micro-dollars at load time: in the rest of the
/// program there is no `f64` that touches a price.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceDollars {
    /// Dollars per million input tokens.
    pub input: f64,
    /// Dollars per million output tokens.
    pub output: f64,
}

impl PriceDollars {
    /// The conversion into micro-dollars, or `None` if the price is negative or not
    /// finite.
    #[must_use]
    pub fn to_micro(self) -> Option<(crate::pricing::MicroUsd, crate::pricing::MicroUsd)> {
        crate::pricing::usd(self.input).zip(crate::pricing::usd(self.output))
    }
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
const fn default_timeout_ms() -> u64 {
    60_000
}
const fn default_max_attempts() -> u32 {
    4
}
const fn default_priority() -> u32 {
    100
}
const fn default_sample_rate() -> u64 {
    100
}

/// A configuration problem, with the place where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Path in the file, e.g. `tenants[0].monthly_budget_usd`.
    pub field: String,
    /// What is wrong, in one sentence.
    pub problem: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.problem)
    }
}

impl std::error::Error for ConfigError {}

/// The result of loading and validating a configuration.
#[derive(Debug)]
pub struct LoadedConfig {
    /// The configuration with prices already converted into integer micro-dollars.
    pub config: ValidatedConfig,
    /// The tenant keys read from the environment, indexed by tenant id.
    pub keys: BTreeMap<String, String>,
    /// The provider keys read from the environment, indexed by provider name.
    pub provider_keys: BTreeMap<String, String>,
}

/// Validated configuration: past this point the fields are consistent by construction.
#[derive(Debug)]
pub struct ValidatedConfig {
    /// Server settings.
    pub server: ServerConfig,
    /// Providers, **already sorted** by priority.
    pub providers: Vec<ProviderConfig>,
    /// Prices in integer micro-dollars.
    pub pricing: PriceTable,
    /// Declared tenants, in file order.
    pub tenants: Vec<TenantConfig>,
    /// Tenants by identifier, for O(1) lookup on the hot path.
    pub tenants_by_id: BTreeMap<String, usize>,
}

/// Loads from a TOML string and validates.
pub fn from_toml(source: &str) -> Result<LoadedConfig, Vec<ConfigError>> {
    let config: Config = toml::from_str(source).map_err(|e| {
        vec![ConfigError {
            field: "(file)".to_owned(),
            problem: format!("not valid TOML: {e}"),
        }]
    })?;
    validate(&config, &real_env)
}

/// Loads from a file.
pub fn from_path(path: &Path) -> Result<LoadedConfig, Vec<ConfigError>> {
    let source = std::fs::read_to_string(path).map_err(|e| {
        vec![ConfigError {
            field: path.display().to_string(),
            problem: format!("cannot be read: {e}"),
        }]
    })?;
    from_toml(&source)
}

/// The function that reads environment variables, replaceable in tests.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Validates and resolves the keys from the environment.
///
/// It collects **all** errors instead of stopping at the first one: a configuration with
/// three problems must be fixed in one go, not in three trips.
pub fn validate(config: &Config, env: EnvLookup<'_>) -> Result<LoadedConfig, Vec<ConfigError>> {
    let mut ctx = Context::default();

    check_server(config, &mut ctx);
    let providers = check_providers(config, env, &mut ctx);
    check_pricing(config, &mut ctx);
    check_tenants(config, env, &mut ctx);
    cross_checks(config, &mut ctx);

    if !ctx.errors.is_empty() {
        return Err(ctx.errors);
    }

    // the ordering of the providers is part of the contract: failover follows the
    // declared priority, and without this sort it is not deterministic
    let mut providers = providers;
    providers.sort_by_key(|p| p.priority);

    Ok(LoadedConfig {
        config: ValidatedConfig {
            server: config.server.clone(),
            providers,
            pricing: PriceTable::new(ctx.prices),
            tenants: config.tenants.clone(),
            tenants_by_id: ctx.tenants_by_id,
        },
        keys: ctx.keys,
        provider_keys: ctx.provider_keys,
    })
}

/// The state the validation phases pass to one another.
#[derive(Default)]
struct Context {
    errors: Vec<ConfigError>,
    prices: BTreeMap<String, Price>,
    keys: BTreeMap<String, String>,
    provider_keys: BTreeMap<String, String>,
    tenants_by_id: BTreeMap<String, usize>,
}

impl Context {
    /// Records a problem. It never returns: validation collects, it does not interrupt.
    fn add(&mut self, field: impl Into<String>, problem: impl Into<String>) {
        self.errors.push(ConfigError {
            field: field.into(),
            problem: problem.into(),
        });
    }
}

fn check_server(config: &Config, ctx: &mut Context) {
    if config.server.max_attempts == 0 {
        ctx.add(
            "server.max_attempts",
            "must be at least 1: with 0 no request would pass",
        );
    }
    if config.server.metrics_sample_rate == 0 {
        ctx.add(
            "server.metrics_sample_rate",
            "must be at least 1: with 0 no metric would be sampled",
        );
    }
    if config.server.request_timeout_ms == 0 {
        ctx.add(
            "server.request_timeout_ms",
            "must be at least 1: a null timeout rejects every request",
        );
    }
}

fn check_providers(config: &Config, env: EnvLookup<'_>, ctx: &mut Context) -> Vec<ProviderConfig> {
    if config.providers.is_empty() {
        ctx.add(
            "providers",
            "at least one provider is needed: without one, the gateway cannot forward anything",
        );
    }

    let mut names = BTreeSet::new();
    let mut priorities = BTreeSet::new();

    for (i, p) in config.providers.iter().enumerate() {
        let field = format!("providers[{i}]");

        if p.name.trim().is_empty() {
            ctx.add(
                format!("{field}.name"),
                "the name cannot be empty: it is the key in the logs",
            );
        } else if !names.insert(p.name.clone()) {
            ctx.add(
                format!("{field}.name"),
                format!("provider {:?} is declared twice", p.name),
            );
        }

        if p.base_url.trim().is_empty() {
            ctx.add(format!("{field}.base_url"), "the base URL is required");
        } else if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
            ctx.add(
                format!("{field}.base_url"),
                format!(
                    "{:?} does not look like a URL: http:// or https:// expected",
                    p.base_url
                ),
            );
        }

        if let Some(secret) =
            check_secret(&format!("{field}.api_key_env"), &p.api_key_env, env, ctx)
        {
            ctx.provider_keys.insert(p.name.clone(), secret);
        }

        if !priorities.insert(p.priority) {
            ctx.add(
                format!("{field}.priority"),
                format!(
                    "priority {} is already used: without an order, failover is not deterministic",
                    p.priority
                ),
            );
        }
    }

    config.providers.clone()
}

fn check_pricing(config: &Config, ctx: &mut Context) {
    let mut invalid = Vec::new();
    for (model, p) in &config.pricing {
        match p.to_micro() {
            Some((input, output)) => {
                ctx.prices.insert(model.clone(), Price { input, output });
            }
            None => invalid.push(model.clone()),
        }
    }

    for model in invalid {
        ctx.add(
            format!("pricing.{model}"),
            "negative or non-finite price: a negative price list is a typo, not a discount",
        );
    }

    if ctx.prices.is_empty() {
        ctx.add(
            "pricing",
            "at least one price is needed: without one every request costs zero and the cap protects nothing",
        );
    }
}

fn check_tenants(config: &Config, env: EnvLookup<'_>, ctx: &mut Context) {
    if config.tenants.is_empty() {
        ctx.add(
            "tenants",
            "at least one tenant is needed: without one, nobody can use the gateway",
        );
    }

    for (i, t) in config.tenants.iter().enumerate() {
        let field = format!("tenants[{i}]");

        if t.id.trim().is_empty() {
            ctx.add(
                format!("{field}.id"),
                "the id cannot be empty: it is the key of the metrics",
            );
        } else if ctx.tenants_by_id.contains_key(&t.id) {
            ctx.add(
                format!("{field}.id"),
                format!("tenant {:?} is declared twice", t.id),
            );
        } else {
            ctx.tenants_by_id.insert(t.id.clone(), i);
        }

        match crate::pricing::usd(t.monthly_budget_usd) {
            None => ctx.add(
                format!("{field}.monthly_budget_usd"),
                "the cap must be a positive and finite number",
            ),
            Some(0) => ctx.add(
                format!("{field}.monthly_budget_usd"),
                "a cap of zero allows no requests: if that is intended, remove the tenant",
            ),
            Some(_) => {}
        }

        if let Some(secret) = check_secret(&format!("{field}.key_env"), &t.key_env, env, ctx) {
            ctx.keys.insert(t.id.clone(), secret);
        }
    }
}

/// Resolves a secret from the environment.
///
/// A missing secret is a **startup error**, not a problem to discover on the first
/// request: by then failover has already tried a provider that could not work, and the
/// time lost belongs to the end user.
///
/// It returns the problem as a string instead of writing it: the caller decides where it
/// goes, because providers and tenants have different maps.
fn resolve_secret(
    field: &str,
    env_var: &str,
    env: EnvLookup<'_>,
) -> Result<String, (String, String)> {
    if env_var.trim().is_empty() {
        return Err((
            field.to_owned(),
            "the name of the environment variable with the key is needed".to_owned(),
        ));
    }
    match env(env_var) {
        Some(v) if v.trim().is_empty() => {
            Err((field.to_owned(), format!("variable {env_var} is empty")))
        }
        Some(v) => Ok(v),
        None => Err((
            field.to_owned(),
            format!("environment variable {env_var} is not set"),
        )),
    }
}

/// Like [`resolve_secret`], but the problem goes straight into the collection.
fn check_secret(
    field: &str,
    env_var: &str,
    env: EnvLookup<'_>,
    ctx: &mut Context,
) -> Option<String> {
    match resolve_secret(field, env_var, env) {
        Ok(secret) => Some(secret),
        Err((field, problem)) => {
            ctx.add(field, problem);
            None
        }
    }
}

/// The checks that do not fit inside a single section.
///
/// They are here because they involve two sections at once: this is where the holes a
/// single check cannot find become visible.
fn cross_checks(config: &Config, ctx: &mut Context) {
    for (i, t) in config.tenants.iter().enumerate() {
        if !t.models.is_empty() && t.models.iter().all(|m| !ctx.prices.contains_key(m)) {
            ctx.add(
                format!("tenants[{i}].models"),
                format!(
                    "none of the models {:?} has a declared price: the tenant could spend without being counted",
                    t.models
                ),
            );
        }
    }
}

/// Really reads from the process environment.
#[must_use]
pub fn real_env(name: &str) -> Option<String> {
    env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::usd;

    fn env_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    const BASE: &str = r#"
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
priority = 1

[pricing]
"gpt-4o-mini" = { input = 0.15, output = 0.60 }

[[tenants]]
id = "acme"
key_env = "TENANT_ACME"
monthly_budget_usd = 50.0
"#;

    #[test]
    fn a_minimal_but_sensible_configuration_loads() {
        let keys: [(&str, &str); 2] = [
            ("TENANT_ACME", "test-secret"),
            ("OPENAI_API_KEY", "sk-openai"),
        ];
        let cfg: Config = toml::from_str(BASE).expect("valid TOML");
        let loaded = validate(&cfg, &env_from(&keys)).expect("valid configuration");

        assert_eq!(loaded.config.server.bind, "127.0.0.1:8080");
        assert_eq!(loaded.config.server.max_attempts, 4);
        assert_eq!(
            loaded.config.pricing.resolve("gpt-4o-mini").input,
            usd(0.15).unwrap()
        );
        assert_eq!(
            loaded.keys.get("acme").map(String::as_str),
            Some("test-secret")
        );
    }

    #[test]
    fn the_tenant_is_found_by_id_in_constant_time() {
        let keys: [(&str, &str); 2] = [("TENANT_ACME", "x"), ("OPENAI_API_KEY", "k")];
        let cfg: Config = toml::from_str(BASE).expect("valid TOML");
        let loaded = validate(&cfg, &env_from(&keys)).expect("valid");
        let index = loaded.config.tenants_by_id["acme"];
        assert_eq!(loaded.config.tenants[index].id, "acme");
    }

    #[test]
    fn providers_are_sorted_by_priority() {
        let toml = r#"
[[providers]]
name = "slow"
base_url = "https://slow.example.com/v1"
api_key_env = "K_SLOW"
priority = 10

[[providers]]
name = "fast"
base_url = "https://fast.example.com/v1"
api_key_env = "K_FAST"
priority = 1

[pricing]
"m" = { input = 1.0, output = 2.0 }

[[tenants]]
id = "acme"
key_env = "K_TENANT"
monthly_budget_usd = 10.0
"#;
        let keys: [(&str, &str); 3] = [("K_SLOW", "a"), ("K_FAST", "b"), ("K_TENANT", "c")];
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let loaded = validate(&cfg, &env_from(&keys)).expect("valid");
        assert_eq!(loaded.config.providers[0].name, "fast");
        assert_eq!(loaded.config.providers[1].name, "slow");
    }

    #[test]
    fn a_key_that_does_not_exist_is_a_startup_error() {
        let cfg: Config = toml::from_str(BASE).expect("valid TOML");
        let errors =
            validate(&cfg, &env_from(&[])).expect_err("the environment variables are missing");

        // both the tenant one and the provider one: a missing key discovered on the
        // first request wastes the end user's time
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|e| e.field == "tenants[0].key_env"));
        assert!(errors.iter().any(|e| e.field == "providers[0].api_key_env"));
        assert!(errors.iter().all(|e| e.problem.contains("is not set")));
    }

    #[test]
    fn all_errors_are_collected_not_one_at_a_time() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "not-a-url"
api_key_env = ""

[pricing]

[[tenants]]
id = "x"
key_env = "MISSING"
monthly_budget_usd = -5.0
"#;
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let errors = validate(&cfg, &env_from(&[])).expect_err("many problems");
        // base_url, api_key_env, empty pricing, missing key_env, negative budget
        assert!(
            errors.len() >= 5,
            "only {} errors collected: {errors:?}",
            errors.len()
        );
    }

    #[test]
    fn a_negative_price_is_a_typo_not_a_discount() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "K"

[pricing]
"m" = { input = -1.0, output = 2.0 }

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = 10.0
"#;
        let keys: [(&str, &str); 1] = [("K", "secret")];
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let errors = validate(&cfg, &env_from(&keys)).expect_err("negative price");
        assert!(errors.iter().any(|e| e.field == "pricing.m"));
    }

    #[test]
    fn an_empty_price_table_would_disable_the_cap() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "K"

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = 10.0
"#;
        let keys: [(&str, &str); 1] = [("K", "secret")];
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let errors = validate(&cfg, &env_from(&keys)).expect_err("no price");
        assert!(errors.iter().any(|e| e.problem.contains("costs zero")));
    }

    #[test]
    fn two_providers_with_the_same_priority_have_no_order() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "K"
priority = 1

[[providers]]
name = "b"
base_url = "https://b.example.com"
api_key_env = "K"
priority = 1

[pricing]
"m" = { input = 1.0, output = 1.0 }

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = 10.0
"#;
        let keys: [(&str, &str); 1] = [("K", "secret")];
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let errors = validate(&cfg, &env_from(&keys)).expect_err("ambiguous priority");
        assert!(errors
            .iter()
            .any(|e| e.problem.contains("not deterministic")));
    }

    #[test]
    fn a_provider_key_that_does_not_exist_is_a_problem() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "DOES_NOT_EXIST"

[pricing]
"m" = { input = 1.0, output = 1.0 }

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = 10.0
"#;
        let keys: [(&str, &str); 1] = [("K", "secret")];
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let errors = validate(&cfg, &env_from(&keys)).expect_err("missing provider key");
        assert!(errors.iter().any(|e| e.field == "providers[0].api_key_env"));
    }

    #[test]
    fn a_tenant_that_can_only_use_unpriced_models_is_muted() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "K"

[pricing]
"a-priced" = { input = 1.0, output = 1.0 }

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = 10.0
models = ["a-priced", "b-unpriced"]
"#;
        let keys: [(&str, &str); 1] = [("K", "secret")];
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let loaded = validate(&cfg, &env_from(&keys)).expect("it has at least one priced model");
        assert!(loaded.config.tenants[0].models.contains("b-unpriced"));
    }

    #[test]
    fn a_negative_monthly_cap_is_not_a_cap() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "K"

[pricing]
"m" = { input = 1.0, output = 1.0 }

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = -1.0
"#;
        let keys: [(&str, &str); 1] = [("K", "secret")];
        let cfg: Config = toml::from_str(toml).expect("valid TOML");
        let errors = validate(&cfg, &env_from(&keys)).expect_err("negative cap");
        assert!(errors
            .iter()
            .any(|e| e.field == "tenants[0].monthly_budget_usd"));
    }

    #[test]
    fn an_unknown_field_is_an_error_and_not_a_silent_typo() {
        let toml = format!("{BASE}\nunknown_word = 1\n");
        let errors = from_toml(&toml).expect_err("unknown field");
        assert!(errors[0].problem.contains("not valid TOML"));
    }
}
