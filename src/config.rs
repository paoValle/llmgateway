//! La configurazione: come si dichiara un gateway.
//!
//! Un principio solo, che vale per tutto il file: **nessun segreto qui dentro.** Le
//! chiavi si prendono dall'ambiente, e la configurazione dice *dove* cercarle. Il
//! file è committabile, il diff è leggibile, e non c'è un `api_key = "sk-..."` che
//! qualcuno lascia in un repository.
//!
//! La validazione **raccoglie tutti** gli errori prima di dichiararli. Una
//! configurazione che si ferma al primo problema obbliga a un viaggio di andata e
//! ritorno per ogni errore; una che li elenca tutti fa il lavoro una volta sola.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::Path;

use serde::Deserialize;

use crate::pricing::{Price, PriceTable};

/// La configurazione completa.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Dove ascolta e quanto tollera. Di default: valori ragionevoli.
    #[serde(default)]
    pub server: ServerConfig,
    /// I provider, in ordine di preferenza.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// I prezzi per modello, in dollari per milione di token.
    #[serde(default)]
    pub pricing: BTreeMap<String, PriceDollars>,
    /// I tenant che possono usare il gateway.
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
}

/// `[server]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Indirizzo di ascolto.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Tetto di attesa per una chiamata upstream. 60 s di default: sotto, un
    /// provider lento sembra morto.
    #[serde(default = "default_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Tetto di tentativi **complessivi**, su tutti i provider.
    ///
    /// Il failover che non ha un tetto è un attacco che si autoalimenta (`DDoS`): ogni provider
    /// che non risponde porta a provare il successivo, e se i provider non
    /// rispondono tutti si moltiplica il carico proprio quando è già al limite.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Tasso di campionamento delle metriche operative: 1 su N. 1 = esatte.
    #[serde(default = "default_sample_rate")]
    pub metrics_sample_rate: u64,
}

impl Default for ServerConfig {
    /// Scritto a mano e non derivato: i default qui sono scelte, non valori neutri,
    /// e `derive(Default)` darebbe zeri — un bind a `0.0.0.0:0` e un timeout nullo.
    fn default() -> Self {
        Self {
            bind: default_bind(),
            request_timeout_ms: default_timeout_ms(),
            max_attempts: default_max_attempts(),
            metrics_sample_rate: default_sample_rate(),
        }
    }
}

/// Un `[[providers]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Nome usato nei log e nelle metriche.
    pub name: String,
    /// Base URL dell'API, senza lo `/chat/completions`.
    pub base_url: String,
    /// Variabile d'ambiente che contiene la chiave. **Non** la chiave.
    pub api_key_env: String,
    /// Ordine di preferenza: più basso prima.
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Modelli che questo provider serve. Vuoto = "tutti quelli che dichiara il tenant".
    #[serde(default)]
    pub models: BTreeSet<String>,
}

/// Un `[[tenants]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    /// Identificatore del tenant: compare nelle metriche e nei log.
    pub id: String,
    /// Variabile d'ambiente che contiene la chiave di questo tenant.
    ///
    /// Il gateway la cerca in arrivo e ne confronta il valore. La chiave non
    /// transita in nessun log (vedi `redact.rs`).
    pub key_env: String,
    /// Tetto mensile, in dollari.
    pub monthly_budget_usd: f64,
    /// Modelli che questo tenant può usare. Vuoto = tutti.
    #[serde(default)]
    pub models: BTreeSet<String>,
}

/// Prezzi nel formato che si scrive a mano: dollari per milione di token.
///
/// `serde` lo converte in micro-dollari interi al momento del caricamento: nel resto
/// del programma non esiste un `f64` che tocchi un prezzo.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceDollars {
    /// Dollari per milione di token di input.
    pub input: f64,
    /// Dollari per milione di token di output.
    pub output: f64,
}

impl PriceDollars {
    /// La conversione in micro-dollari, o `None` se il prezzo è negativo o non finito.
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

/// Un problema della configurazione, con il posto in cui si trova.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Percorso nel file, es. `tenants[0].monthly_budget_usd`.
    pub field: String,
    /// Cosa non torna, in una frase.
    pub problem: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.problem)
    }
}

impl std::error::Error for ConfigError {}

/// Il risultato di caricare e validare una configurazione.
#[derive(Debug)]
pub struct LoadedConfig {
    /// La configurazione con i prezzi già convertiti in micro-dollari interi.
    pub config: ValidatedConfig,
    /// Le chiavi dei tenant lette dall'ambiente, indicizzate per id tenant.
    pub keys: BTreeMap<String, String>,
    /// Le chiavi dei provider lette dall'ambiente, indicizzate per nome provider.
    pub provider_keys: BTreeMap<String, String>,
}

/// Configurazione validata: dopo questo punto i campi sono coerenti per costruzione.
#[derive(Debug)]
pub struct ValidatedConfig {
    /// Impostazioni del server.
    pub server: ServerConfig,
    /// Provider, **già ordinati** per priorità.
    pub providers: Vec<ProviderConfig>,
    /// Prezzi in micro-dollari interi.
    pub pricing: PriceTable,
    /// Tenant dichiarati, nell'ordine del file.
    pub tenants: Vec<TenantConfig>,
    /// Tenant per identificatore, per lookup in O(1) nel percorso caldo.
    pub tenants_by_id: BTreeMap<String, usize>,
}

/// Carica da una stringa TOML e valida.
pub fn from_toml(source: &str) -> Result<LoadedConfig, Vec<ConfigError>> {
    let config: Config = toml::from_str(source).map_err(|e| {
        vec![ConfigError {
            field: "(file)".to_owned(),
            problem: format!("non è TOML valido: {e}"),
        }]
    })?;
    validate(&config, &real_env)
}

/// Carica da un file.
pub fn from_path(path: &Path) -> Result<LoadedConfig, Vec<ConfigError>> {
    let source = std::fs::read_to_string(path).map_err(|e| {
        vec![ConfigError {
            field: path.display().to_string(),
            problem: format!("non si può leggere: {e}"),
        }]
    })?;
    from_toml(&source)
}

/// La funzione che legge le variabili d'ambiente, sostituibile nei test.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Valida e risolve le chiavi dall'ambiente.
///
/// Raccolge **tutti** gli errori invece di fermarsi al primo: una configurazione con
/// tre problemi va corretta in un colpo, non in tre viaggi.
pub fn validate(config: &Config, env: EnvLookup<'_>) -> Result<LoadedConfig, Vec<ConfigError>> {
    let mut ctx = Context::default();

    check_server(config, &mut ctx);
    let providers = check_providers(config, env, &mut ctx);
    check_pricing(config, &mut ctx);
    check_tenants(config, env, &mut ctx);
    cross_checks(config, &mut ctx);

    if !ctx.errori.is_empty() {
        return Err(ctx.errori);
    }

    // l'ordinamento dei provider è parte del contratto: il failover segue la
    // priorità dichiarata, e senza questo sort non è deterministico
    let mut providers = providers;
    providers.sort_by_key(|p| p.priority);

    Ok(LoadedConfig {
        config: ValidatedConfig {
            server: config.server.clone(),
            providers,
            pricing: PriceTable::new(ctx.prezzi),
            tenants: config.tenants.clone(),
            tenants_by_id: ctx.tenants_by_id,
        },
        keys: ctx.keys,
        provider_keys: ctx.provider_keys,
    })
}

/// Lo stato che le fasi di validazione si passano.
#[derive(Default)]
struct Context {
    errori: Vec<ConfigError>,
    prezzi: BTreeMap<String, Price>,
    keys: BTreeMap<String, String>,
    provider_keys: BTreeMap<String, String>,
    tenants_by_id: BTreeMap<String, usize>,
}

impl Context {
    /// Segna un problema. Non ritorna mai: la validazione raccoglie, non interrompe.
    fn add(&mut self, field: impl Into<String>, problem: impl Into<String>) {
        self.errori.push(ConfigError {
            field: field.into(),
            problem: problem.into(),
        });
    }
}

fn check_server(config: &Config, ctx: &mut Context) {
    if config.server.max_attempts == 0 {
        ctx.add(
            "server.max_attempts",
            "deve essere almeno 1: con 0 nessuna richiesta passerebbe",
        );
    }
    if config.server.metrics_sample_rate == 0 {
        ctx.add(
            "server.metrics_sample_rate",
            "deve essere almeno 1: con 0 nessuna metrica sarebbe campionata",
        );
    }
    if config.server.request_timeout_ms == 0 {
        ctx.add(
            "server.request_timeout_ms",
            "deve essere almeno 1: un timeout nullo rifiuta ogni richiesta",
        );
    }
}

fn check_providers(config: &Config, env: EnvLookup<'_>, ctx: &mut Context) -> Vec<ProviderConfig> {
    if config.providers.is_empty() {
        ctx.add(
            "providers",
            "serve almeno un provider: senza, il gateway non può inoltrare nulla",
        );
    }

    let mut nomi = BTreeSet::new();
    let mut priorità = BTreeSet::new();

    for (i, p) in config.providers.iter().enumerate() {
        let campo = format!("providers[{i}]");

        if p.name.trim().is_empty() {
            ctx.add(
                format!("{campo}.name"),
                "il nome non può essere vuoto: è la chiave nei log",
            );
        } else if !nomi.insert(p.name.clone()) {
            ctx.add(
                format!("{campo}.name"),
                format!("il provider {:?} è dichiarato due volte", p.name),
            );
        }

        if p.base_url.trim().is_empty() {
            ctx.add(format!("{campo}.base_url"), "la base URL è obbligatoria");
        } else if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
            ctx.add(
                format!("{campo}.base_url"),
                format!(
                    "{:?} non sembra un URL: attesi http:// o https://",
                    p.base_url
                ),
            );
        }

        if let Some(segreto) =
            check_secret(&format!("{campo}.api_key_env"), &p.api_key_env, env, ctx)
        {
            ctx.provider_keys.insert(p.name.clone(), segreto);
        }

        if !priorità.insert(p.priority) {
            ctx.add(
                format!("{campo}.priority"),
                format!(
                    "la priorità {} è già usuta: senza un ordine, il failover non è deterministico",
                    p.priority
                ),
            );
        }
    }

    config.providers.clone()
}

fn check_pricing(config: &Config, ctx: &mut Context) {
    let mut invalidi = Vec::new();
    for (modello, p) in &config.pricing {
        match p.to_micro() {
            Some((input, output)) => {
                ctx.prezzi.insert(modello.clone(), Price { input, output });
            }
            None => invalidi.push(modello.clone()),
        }
    }

    for modello in invalidi {
        ctx.add(
            format!("pricing.{modello}"),
            "prezzo negativo o non finito: un listino negativo è un errore di battitura, non uno sconto",
        );
    }

    if ctx.prezzi.is_empty() {
        ctx.add(
            "pricing",
            "serve almeno un prezzo: senza, ogni richiesta costa zero e il tetto non protegge niente",
        );
    }
}

fn check_tenants(config: &Config, env: EnvLookup<'_>, ctx: &mut Context) {
    if config.tenants.is_empty() {
        ctx.add(
            "tenants",
            "serve almeno un tenant: senza, nessuno può usare il gateway",
        );
    }

    for (i, t) in config.tenants.iter().enumerate() {
        let campo = format!("tenants[{i}]");

        if t.id.trim().is_empty() {
            ctx.add(
                format!("{campo}.id"),
                "l'id non può essere vuoto: è la chiave delle metriche",
            );
        } else if ctx.tenants_by_id.contains_key(&t.id) {
            ctx.add(
                format!("{campo}.id"),
                format!("il tenant {:?} è dichiarato due volte", t.id),
            );
        } else {
            ctx.tenants_by_id.insert(t.id.clone(), i);
        }

        match crate::pricing::usd(t.monthly_budget_usd) {
            None => ctx.add(
                format!("{campo}.monthly_budget_usd"),
                "il tetto deve essere un numero positivo e finito",
            ),
            Some(0) => ctx.add(
                format!("{campo}.monthly_budget_usd"),
                "un tetto di zero non consente richieste: se è voluto, rimuovi il tenant",
            ),
            Some(_) => {}
        }

        if let Some(segreto) = check_secret(&format!("{campo}.key_env"), &t.key_env, env, ctx) {
            ctx.keys.insert(t.id.clone(), segreto);
        }
    }
}

/// Risolve un segreto dall'ambiente.
///
/// Un segreto che manca è un **errore di avvio**, non un problema da scoprire alla
/// prima richiesta: a quel punto il failover ha già provato un provider che non
/// poteva funzionare, e il tempo perso è dell'utente finale.
///
/// Restituisce il problema come stringa invece di scriverlo: chi chiama decide dove
/// finisce, perché provider e tenant hanno mappe diverse.
fn resolve_secret(
    campo: &str,
    env_var: &str,
    env: EnvLookup<'_>,
) -> Result<String, (String, String)> {
    if env_var.trim().is_empty() {
        return Err((
            campo.to_owned(),
            "serve il nome della variabile d'ambiente con la chiave".to_owned(),
        ));
    }
    match env(env_var) {
        Some(v) if v.trim().is_empty() => {
            Err((campo.to_owned(), format!("la variabile {env_var} è vuota")))
        }
        Some(v) => Ok(v),
        None => Err((
            campo.to_owned(),
            format!("la variabile d'ambiente {env_var} non è impostata"),
        )),
    }
}

/// Come [`resolve_secret`], ma il problema finisce subito nella raccolta.
fn check_secret(
    campo: &str,
    env_var: &str,
    env: EnvLookup<'_>,
    ctx: &mut Context,
) -> Option<String> {
    match resolve_secret(campo, env_var, env) {
        Ok(segreto) => Some(segreto),
        Err((campo, problema)) => {
            ctx.add(campo, problema);
            None
        }
    }
}

/// I controlli che non stanno dentro una sezione sola.
///
/// Sono qui perché riguardano due sezioni insieme: è il posto in cui si vedono i
/// buchi che un controllo secco non trova.
fn cross_checks(config: &Config, ctx: &mut Context) {
    for (i, t) in config.tenants.iter().enumerate() {
        if !t.models.is_empty() && t.models.iter().all(|m| !ctx.prezzi.contains_key(m)) {
            ctx.add(
                format!("tenants[{i}].models"),
                format!(
                    "nessuno dei modelli {:?} ha un prezzo dichiarato: il tenant potrebbe spendere senza essere conteggiato",
                    t.models
                ),
            );
        }
    }
}

/// Legge davvero dall'ambiente del processo.
#[must_use]
pub fn real_env(name: &str) -> Option<String> {
    env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::usd;

    fn ambiente(variabili: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let mappa: BTreeMap<String, String> = variabili
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |nome: &str| mappa.get(nome).cloned()
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
    fn una_configurazione_minima_ma_sensata_si_carica() {
        let chiavi: [(&str, &str); 2] = [
            ("TENANT_ACME", "segreto-di-prova"),
            ("OPENAI_API_KEY", "sk-openai"),
        ];
        let cfg: Config = toml::from_str(BASE).expect("TOML valido");
        let loaded = validate(&cfg, &ambiente(&chiavi)).expect("configurazione valida");

        assert_eq!(loaded.config.server.bind, "127.0.0.1:8080");
        assert_eq!(loaded.config.server.max_attempts, 4);
        assert_eq!(
            loaded.config.pricing.resolve("gpt-4o-mini").input,
            usd(0.15).unwrap()
        );
        assert_eq!(
            loaded.keys.get("acme").map(String::as_str),
            Some("segreto-di-prova")
        );
    }

    #[test]
    fn il_tenant_si_trova_per_id_in_tempo_costante() {
        let chiavi: [(&str, &str); 2] = [("TENANT_ACME", "x"), ("OPENAI_API_KEY", "k")];
        let cfg: Config = toml::from_str(BASE).expect("TOML valido");
        let loaded = validate(&cfg, &ambiente(&chiavi)).expect("valida");
        let indice = loaded.config.tenants_by_id["acme"];
        assert_eq!(loaded.config.tenants[indice].id, "acme");
    }

    #[test]
    fn i_provider_vengono_ordinati_per_priorità() {
        let toml = r#"
[[providers]]
name = "lento"
base_url = "https://lento.example.com/v1"
api_key_env = "K_LENTO"
priority = 10

[[providers]]
name = "veloce"
base_url = "https://veloce.example.com/v1"
api_key_env = "K_VELOCE"
priority = 1

[pricing]
"m" = { input = 1.0, output = 2.0 }

[[tenants]]
id = "acme"
key_env = "K_TENANT"
monthly_budget_usd = 10.0
"#;
        let chiavi: [(&str, &str); 3] = [("K_LENTO", "a"), ("K_VELOCE", "b"), ("K_TENANT", "c")];
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let loaded = validate(&cfg, &ambiente(&chiavi)).expect("valida");
        assert_eq!(loaded.config.providers[0].name, "veloce");
        assert_eq!(loaded.config.providers[1].name, "lento");
    }

    #[test]
    fn una_chiave_che_non_esiste_è_un_errore_di_avvio() {
        let cfg: Config = toml::from_str(BASE).expect("TOML valido");
        let errori = validate(&cfg, &ambiente(&[])).expect_err("mancano le variabili d'ambiente");

        // sia quella del tenant sia quella del provider: una chiave mancante
        // scoperta alla prima richiesta fa perdere tempo all'utente finale
        assert_eq!(errori.len(), 2);
        assert!(errori.iter().any(|e| e.field == "tenants[0].key_env"));
        assert!(errori.iter().any(|e| e.field == "providers[0].api_key_env"));
        assert!(errori.iter().all(|e| e.problem.contains("non è impostata")));
    }

    #[test]
    fn tutti_gli_errori_vengono_raccolti_non_uno_alla_volta() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "non-una-url"
api_key_env = ""

[pricing]

[[tenants]]
id = "x"
key_env = "MANCANTE"
monthly_budget_usd = -5.0
"#;
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let errori = validate(&cfg, &ambiente(&[])).expect_err("tanti problemi");
        // base_url, api_key_env, pricing vuota, key_env mancante, budget negativo
        assert!(
            errori.len() >= 5,
            "raccolti solo {} errori: {errori:?}",
            errori.len()
        );
    }

    #[test]
    fn un_prezzo_negativo_è_errore_di_battitura_nonuno_sconto() {
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
        let chiavi: [(&str, &str); 1] = [("K", "segreto")];
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let errori = validate(&cfg, &ambiente(&chiavi)).expect_err("prezzo negativo");
        assert!(errori.iter().any(|e| e.field == "pricing.m"));
    }

    #[test]
    fn una_tabella_prezzi_vuota_disattiverebbe_il_tetto() {
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
        let chiavi: [(&str, &str); 1] = [("K", "segreto")];
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let errori = validate(&cfg, &ambiente(&chiavi)).expect_err("nessun prezzo");
        assert!(errori.iter().any(|e| e.problem.contains("costa zero")));
    }

    #[test]
    fn due_provider_con_la_stessa_priorita_non_hanno_un_ordine() {
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
        let chiavi: [(&str, &str); 1] = [("K", "segreto")];
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let errori = validate(&cfg, &ambiente(&chiavi)).expect_err("priorità ambigua");
        assert!(errori
            .iter()
            .any(|e| e.problem.contains("non è deterministico")));
    }

    #[test]
    fn una_chiave_di_provider_che_non_esiste_e_un_problema() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "NON_ESISTE"

[pricing]
"m" = { input = 1.0, output = 1.0 }

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = 10.0
"#;
        let chiavi: [(&str, &str); 1] = [("K", "segreto")];
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let errori = validate(&cfg, &ambiente(&chiavi)).expect_err("chiave provider mancante");
        assert!(errori.iter().any(|e| e.field == "providers[0].api_key_env"));
    }

    #[test]
    fn un_tenant_che_puo_usare_solo_modelli_senza_prezzo_e_muto() {
        let toml = r#"
[[providers]]
name = "a"
base_url = "https://a.example.com"
api_key_env = "K"

[pricing]
"a-prezzato" = { input = 1.0, output = 1.0 }

[[tenants]]
id = "x"
key_env = "K"
monthly_budget_usd = 10.0
models = ["a-prezzato", "b-non-prezzato"]
"#;
        let chiavi: [(&str, &str); 1] = [("K", "segreto")];
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let loaded = validate(&cfg, &ambiente(&chiavi)).expect("ha almeno un modello prezzato");
        assert!(loaded.config.tenants[0].models.contains("b-non-prezzato"));
    }

    #[test]
    fn un_tetto_mensile_negativo_non_e_un_tetto() {
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
        let chiavi: [(&str, &str); 1] = [("K", "segreto")];
        let cfg: Config = toml::from_str(toml).expect("TOML valido");
        let errori = validate(&cfg, &ambiente(&chiavi)).expect_err("tetto negativo");
        assert!(errori
            .iter()
            .any(|e| e.field == "tenants[0].monthly_budget_usd"));
    }

    #[test]
    fn una_campo_ignoto_e_un_errore_e_non_un_typo_silenzioso() {
        let toml = format!("{BASE}\nparola_inventata = 1\n");
        let errori = from_toml(&toml).expect_err("campo sconosciuto");
        assert!(errori[0].problem.contains("non è TOML valido"));
    }
}
