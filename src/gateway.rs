//! Il gateway: mettere in fila autenticazione, tetto, failover e conto.
//!
//! `Gateway::gestisci` è una **funzione pura** che va da una richiesta a una
//! risposta. Non c'è un `Request` HTTP dentro e non c'è un `Response` HTTP fuori:
//! l'adapter di trasporto sta in [`crate::http`] ed è di poche righe.
//!
//! La ragione è la stessa di `agentloop`: un ciclo che chiama il mondo esterno è
//! testabile solo se il mondo esterno è un'interfaccia. Qui il test con un provider
//! finto e un `BudgetRegistry` costruito a mano verifica **tutto** il percorso —
//! autenticazione, tetto, failover, conto — in microsecondi, senza un server.
//!
//! L'ordine delle operazioni non è casuale, e ognuno dei quattro passi è lì per un
//! motivo:
//!
//! 1. **autenticare**: chi è, e può usare questo modello.
//! 2. **leggere il body e stimare**: quanto costerebbe *prima* di chiamare chiunque.
//! 3. **prenotare il tetto**: se non entra, si risponde `429` e **nessun provider
//!    viene chiamato**. È la differenza fra un tetto e un rendiconto.
//! 4. **inoltrare e saldare**: col conto reale, e la differenza viene liberata.

use std::sync::Arc;
use std::time::Duration;

use crate::auth::{Autenticatore, ErroreAuth};
use crate::budget::{BudgetRegistry, ReserveError};
use crate::meter::{Esito, Meter};
use crate::pricing::{MicroUsd, PriceTable, Usage};
use crate::request::{inspect, RequestShape};
use crate::router::{RouteError, Routed, Router};
use crate::upstream::{ResponseBody, UpstreamRequest};

/// Una richiesta che arriva al gateway. Non è un `Request` HTTP: è ciò che il
/// gateway **usa**, e nulla di più.
#[derive(Debug, Clone)]
pub struct Richiesta {
    /// La chiave del tenant, se presente. `None` è una richiesta non autenticata.
    pub chiave: Option<String>,
    /// Il corpo della richiesta, byte per byte.
    pub body: Vec<u8>,
}

/// Una risposta del gateway. Come la richiesta, non è un `Response` HTTP.
pub enum Risposta {
    /// Risposta in memoria, pronta da servire.
    Intera {
        /// Lo stato.
        status: u16,
        /// Il corpo, **esattamente come è arrivato** dal provider.
        body: Vec<u8>,
        /// Chi ha risposto, se qualcuno ha risposto.
        provider: Option<String>,
    },
    /// Risposta in streaming: il gateway non l'ha accumulata e non deve.
    Flusso {
        /// Lo stato.
        status: u16,
        /// I chunk, nell'ordine in cui arrivano.
        chunks: crate::upstream::ByteStream,
        /// Chi ha risposto.
        provider: String,
    },
}

/// `Debug` a mano per lo stesso motivo di [`crate::upstream::ResponseBody`]: il
/// contenuto di uno streaming non è formattabile, e stamparlo in un log finirebbe
/// con dentro la risposta del provider.
impl std::fmt::Debug for Risposta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Intera {
                status,
                body,
                provider,
            } => f
                .debug_struct("Intera")
                .field("status", status)
                .field("bytes", &body.len())
                .field("provider", provider)
                .finish(),
            Self::Flusso {
                status, provider, ..
            } => f
                .debug_struct("Flusso")
                .field("status", status)
                .field("provider", provider)
                .finish(),
        }
    }
}

impl Risposta {
    /// Lo stato, in entrambi i casi.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Intera { status, .. } | Self::Flusso { status, .. } => *status,
        }
    }

    /// Chi ha risposto, se qualcuno ha risposto.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Intera { provider, .. } => provider.as_deref(),
            Self::Flusso { provider, .. } => Some(provider),
        }
    }
}

/// Come è costruito il gateway.
pub struct GatewayConfig {
    /// Chi può usare il gateway.
    pub autenticatore: Autenticatore,
    /// I tetti per tenant.
    pub budget: BudgetRegistry,
    /// Il contatore.
    pub meter: Arc<Meter>,
    /// Il router.
    pub router: Arc<Router>,
    /// I prezzi per modello, per stimare prima della chiamata.
    pub prezzi: PriceTable,
    /// Output massimo presumed quando il client non lo dichiara.
    pub max_output_default: u64,
    /// Il timeout di una chiamata upstream.
    pub timeout: Duration,
    /// Una funzione che restituisce "adesso", in millisecondi Unix.
    ///
    /// È un parametro e non un `SystemTime::now()` nascosto: i test devono poter
    /// cambiare mese senza dormire trenta giorni.
    pub adesso: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("timeout", &self.timeout)
            .field("max_output_default", &self.max_output_default)
            .finish_non_exhaustive()
    }
}

/// Il gateway.
#[derive(Clone, Debug)]
pub struct Gateway {
    config: Arc<GatewayConfig>,
}

impl Gateway {
    /// Costruisce il gateway. La configurazione è economica da clonare, ma si condivide.
    #[must_use]
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Gestisce una richiesta.
    ///
    /// Non ritorna mai `Err`: ogni fallimento è una risposta, perché dal punto di
    /// vista del client **tutto** è una risposta. Un `Err` qui significherebbe che
    /// il gateway è caduto, e in quel caso l'unica risposta giusta è farlo notare.
    pub async fn gestisci(&self, richiesta: Richiesta) -> Risposta {
        let now = (self.config.adesso)();

        // --- 1. autenticazione ---
        let tenant = match self
            .config
            .autenticatore
            .identifica(richiesta.chiave.as_deref())
        {
            Ok(t) => t,
            Err(e) => {
                // una chiave non valida non è un errore da lograre come guasto: arriva
                // a ogni richiesta se qualcuno ha sbagliato la configurazione
                tracing::info!(errore = %e, "richiesta non autenticata");
                return Self::rifiuta(e);
            }
        };

        // --- 2. lettura del body e stima ---
        let forma = inspect(&richiesta.body, self.config.max_output_default);
        let Some(modello) = forma.model.clone() else {
            // nessun modello: non c'è prezzo e non c'è routing. Rispondere 400 qui è
            // un giudizio sul body che il gateway **non** fa: lo lascia al provider
            tracing::info!(tenant = %tenant.id, "richiesta senza modello leggibile");
            return Self::con_provider(
                400,
                b"{\"error\":{\"message\":\"missing model\"}}".to_vec(),
                None,
            );
        };

        if !self.config.autenticatore.puo_usare(&tenant.id, &modello) {
            tracing::info!(tenant = %tenant.id, model = %modello, "modello non autorizzato per il tenant");
            self.config.meter.registr_tetto_negato(&tenant.id);
            return Self::con_provider(
                403,
                br#"{"error":{"message":"model not allowed for this tenant","type":"permission_error"}}"#.to_vec(),
                None,
            );
        }

        let tetto = self.config.budget.get(&tenant.id);
        let Some(tetto) = tetto else {
            // un tenant autenticato senza tetto è una configurazione incoerente:
            // continuare significherebbe spendere senza controllo
            tracing::error!(tenant = %tenant.id, "tenant senza tetto registrato");
            return Self::con_provider(
                500,
                b"{\"error\":{\"message\":\"tenant has no budget\"}}".to_vec(),
                None,
            );
        };

        let (prezzo, stimato) = self.config.meter.prezzo_per(&modello);
        let stima = prezzo.cost(
            forma.estimated_input_tokens,
            forma.max_output_tokens.unwrap_or(0),
        );

        // --- 3. prenotazione: il tetto vale PRIMA della spesa ---
        let prenotazione = match tetto.reserve(stima, now) {
            Ok(p) => p,
            Err(ReserveError::Exceeded(e)) => {
                tracing::warn!(
                    tenant = %tenant.id,
                    richiesto = e.requested,
                    disponibile = e.available,
                    "budget esaurito: 429 senza chiamare alcun provider"
                );
                self.config.meter.registr_tetto_negato(&tenant.id);
                return tetto_negato();
            }
            Err(ReserveError::State(_)) => {
                // lo stato del tetto è compromesso. Si **va avanti**: è un problema
                // del gateway, non del client, e bloccare ogni richiesta per un lock
                // avvelenato peggiorerebbe le cose
                tracing::error!(tenant = %tenant.id, "stato del budget non leggibile: si serve senza tetto");
                return self
                    .senza_prenotazione(&tenant.id, &modello, &forma, now)
                    .await;
            }
        };

        // --- 4. inoltro e saldo ---
        let esito = self
            .inoltra(&tenant.id, &modello, &forma, richiesta.body)
            .await;

        match esito {
            EsitoRotta::Servita(routed) => {
                let (status, corpo, uso) = from_response(&routed.response);
                tetto.settle(prenotazione, uso_cost(&uso, prezzo), now);
                self.config.meter.registr(
                    &tenant.id,
                    &forma,
                    uso,
                    &Esito::Servito {
                        provider: routed.provider.clone(),
                        prezzo_stimato: stimato,
                    },
                );
                to_risposta(status, corpo, routed)
            }
            EsitoRotta::Fallita(errore) => {
                // nessun provider ha risposto: la prenotazione non è stata spesa,
                // e va liberata perché il denaro non è uscito
                tetto.release(prenotazione, now);
                self.config.meter.registr(
                    &tenant.id,
                    &forma,
                    Usage::default(),
                    &Esito::Fallito {
                        provider: errore.to_string(),
                    },
                );
                tracing::warn!(tenant = %tenant.id, errore = %errore, "nessun provider ha servito la richiesta");
                Self::risposta_da_errore(&errore)
            }
        }
    }

    /// Il percorso quando lo stato del tetto non è leggibile: si serve senza tetto,
    /// e si dice. È la via che ADR 0004 chiama "il contabile non fa fallire il
    /// servizio", applicata al tetto invece che al conto.
    async fn senza_prenotazione(
        &self,
        tenant: &str,
        modello: &str,
        forma: &RequestShape,
        now: i64,
    ) -> Risposta {
        let _ = now;
        match self.inoltra(tenant, modello, forma, Vec::new()).await {
            EsitoRotta::Servita(routed) => {
                self.config.meter.registr_senza_tetto(tenant);
                let (status, corpo, uso) = from_response(&routed.response);
                let (_, stimato) = self.config.meter.prezzo_per(modello);
                self.config.meter.registr(
                    tenant,
                    forma,
                    uso,
                    &Esito::Servito {
                        provider: routed.provider.clone(),
                        prezzo_stimato: stimato,
                    },
                );
                to_risposta(status, corpo, routed)
            }
            EsitoRotta::Fallita(e) => Self::risposta_da_errore(&e),
        }
    }

    /// Inoltra al router. Non è dentro `gestisci` per una ragione sola: il tentativo
    /// di autenticazione e il tentativo di inoltro non hanno nulla in comune, e
    /// mescolarli renderebbe `gestisci` illeggibile.
    async fn inoltra(
        &self,
        tenant: &str,
        modello: &str,
        forma: &RequestShape,
        body: Vec<u8>,
    ) -> EsitoRotta {
        let _ = tenant;
        let _ = forma;
        let richiesta = UpstreamRequest::new(body, modello.to_owned(), forma.stream);
        match self.config.router.route(richiesta).await {
            Ok(routed) => EsitoRotta::Servita(routed),
            Err(e) => EsitoRotta::Fallita(e),
        }
    }

    /// Una risposta costruita dal gateway, non da un provider.
    fn con_provider(status: u16, body: Vec<u8>, provider: Option<String>) -> Risposta {
        Risposta::Intera {
            status,
            body,
            provider,
        }
    }

    /// Un errore del gateway tradotto in una risposta HTTP.
    fn rifiuta(errore: ErroreAuth) -> Risposta {
        match errore {
            // nessuna chiave: si dice che manca, e si dice anche come si risolve
            ErroreAuth::Mancata => Self::con_provider(
                401,
                br#"{"error":{"message":"missing API key","type":"authentication_error"}}"#
                    .to_vec(),
                None,
            ),
            ErroreAuth::Sconosciuta => Self::con_provider(
                401,
                br#"{"error":{"message":"invalid API key","type":"authentication_error"}}"#
                    .to_vec(),
                None,
            ),
            ErroreAuth::Vuoto => Self::con_provider(
                401,
                br#"{"error":{"message":"empty API key","type":"authentication_error"}}"#.to_vec(),
                None,
            ),
        }
    }

    /// Un errore del router tradotto in una risposta, senza rivelare che esiste
    /// un secondo provider.
    fn risposta_da_errore(errore: &RouteError) -> Risposta {
        match errore {
            // un errore del provider torna al client com'è: è un suo errore, e il
            // suo corpo spiega perché meglio di una risposta scritta dal gateway
            RouteError::ClientFault { status, body, .. }
            | RouteError::UnknownStatus { status, body, .. } => {
                Self::con_provider(*status, body.clone(), None)
            }
            // il provider non poteva, e il prossimo nemmeno: è un fatto del gateway
            RouteError::ProviderUnavailable { .. } => Self::con_provider(
                502,
                br#"{"error":{"message":"all providers failed","type":"api_error"}}"#.to_vec(),
                None,
            ),
            RouteError::NoProviderForModel { model, servibili } => {
                tracing::info!(model = %model, ?servibili, "nessun provider per il modello");
                Self::con_provider(
                    404,
                    format!(
                        r#"{{"error":{{"message":"no provider serves model {model}","type":"not_found_error"}}}}"#
                    )
                    .into_bytes(),
                    None,
                )
            }
            RouteError::NoProviders => Self::con_provider(
                503,
                br#"{"error":{"message":"gateway has no providers configured"}}"#.to_vec(),
                None,
            ),
            // qui non si dice "ho provato tre provider": un cliente non può fare
            // niente con quell'informazione, e un assaltante ne farebbe una mappa
            RouteError::DeliveryUnknown { .. } | RouteError::Exhausted { .. } => {
                Self::con_provider(
                    502,
                    br#"{"error":{"message":"all providers failed","type":"api_error"}}"#.to_vec(),
                    None,
                )
            }
        }
    }
}

/// La risposta a un tetto esaurito.
///
/// Qui la cosa importante non è il corpo ma il fatto che **nessun provider è stato
/// chiamato**: è la differenza fra un tetto e un rendiconto. Un `429` dopo la
/// chiamata avrebbe già speso il denaro che doveva impedire di spendere.
fn tetto_negato() -> Risposta {
    Risposta::Intera {
        status: 429,
        body: br#"{"error":{"message":"monthly budget exhausted","type":"rate_limit_error"}}"#
            .to_vec(),
        provider: None,
    }
}

/// Il corpo di un provider, con l'uso che contiene.
fn from_response(response: &crate::upstream::UpstreamResponse) -> (u16, Vec<u8>, Usage) {
    let status = response.status;
    match &response.body {
        ResponseBody::Buffered(b) => (status, b.clone(), parse_usage(b)),
        // su uno streaming l'uso arriva nell'ultimo chunk e non è disponibile qui:
        // è il motivo per cui il metering di uno streaming va fatto a valle
        ResponseBody::Stream(_) => (status, Vec::new(), Usage::default()),
    }
}

/// Il costo del consumo reale, dato il prezzo già risolto.
fn uso_cost(uso: &Usage, prezzo: crate::pricing::Price) -> MicroUsd {
    uso.cost(prezzo)
}

/// L'uso dichiarato dal provider.
///
/// Se il provider non manda `usage` si conta zero. Il gateway **non** stima a
/// posteriori: un numero inventato dopo il fatto è peggio di uno zero dichiarato,
/// e in entrambi i casi la fattura del provider resta la fonte autorevole.
fn parse_usage(body: &[u8]) -> Usage {
    match serde_json::from_slice::<RawUsage>(body) {
        Ok(u) => {
            let uso = u.usage.unwrap_or_default();
            Usage {
                input_tokens: uso.prompt_tokens,
                output_tokens: uso.completion_tokens,
            }
        }
        Err(_) => Usage::default(),
    }
}

/// La forma con cui il provider dichiara l'uso. I nomi sono i suoi, non i nostri:
/// `Usage` ha `input_tokens`/`output_tokens` perché sono nomi migliori, e la
/// traduzione sta qui e non nel tipo che il resto del gateway usa.
#[derive(serde::Deserialize)]
struct RawUsage {
    #[serde(default)]
    usage: Option<RawUsageBody>,
}

#[derive(Default, serde::Deserialize)]
struct RawUsageBody {
    #[serde(default, rename = "prompt_tokens")]
    prompt_tokens: u64,
    #[serde(default, rename = "completion_tokens")]
    completion_tokens: u64,
}

/// La risposta del router, o il motivo per cui non c'è stata.
enum EsitoRotta {
    Servita(Routed),
    Fallita(RouteError),
}

/// Converte la risposta del router in quella del gateway, preservando lo streaming.
fn to_risposta(status: u16, corpo: Vec<u8>, routed: Routed) -> Risposta {
    match routed.response.body {
        ResponseBody::Buffered(_) => Risposta::Intera {
            status,
            body: corpo,
            provider: Some(routed.provider),
        },
        ResponseBody::Stream(chunks) => Risposta::Flusso {
            status,
            chunks,
            provider: routed.provider,
        },
    }
}
