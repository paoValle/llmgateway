//! Il router: sceglie il provider e applica il failover.
//!
//! Tutta la politica sta qui e **non sa nulla di HTTP**: `Upstream` è un trait,
//! `TransportError` è un enum. È la conseguenza diretta di ADR 0001 e di ADR 0003 —
//! le regole che decidono sono regole economiche e di dominio, quindi il posto dove
//! stanno non ha niente a che fare con il trasporto.
//!
//! L'algoritmo, in una riga per tentativo:
//!
//! - il provider **non serve** il modello → **saltato**, non è un errore;
//! - la risposta è un successo → restituita, con il nome di chi l'ha servita;
//! - l'errore è **del client** o **sconosciuto** → restituito subito, nessun failover;
//! - l'errore è **del provider** o di trasporto → al successivo, se ne resta e se
//!   ritentare è sicuro.
//!
//! Quell'ultimo "se ritentare è sicuro" è [`Upstream::Delivery`]: una richiesta partita
//! e di esito ignoto non si ritenta, perché un doppio addebito costa più di un errore.

use std::time::Duration;

use tracing::{debug, info, warn};

use crate::upstream::{
    classify, Delivery, FailureClass, ResponseBody, SharedUpstream, TransportError, TransportKind,
    Upstream, UpstreamRequest, UpstreamResponse,
};

/// Tetto di tentativi su tutti i provider, se il chiamante non lo specifica.
///
/// Il failover senza tetto è un attacco che si autoalimenta: quando i provider sono
/// lenti l'uno con l'altro, ogni tentativo aggiunge carico proprio quando ce n'è
/// già troppo.
pub const DEFAULT_MAX_ATTEMPTS: usize = 4;

/// Perché il router non ha potuto servire la richiesta.
#[derive(Debug)]
pub enum RouteError {
    /// Nessun provider dichiara di servire il modello richiesto.
    NoProviderForModel {
        /// Il modello richiesto.
        model: String,
        /// I modelli che i provider disponibili servono davvero.
        servibili: Vec<String>,
    },
    /// Non c'è nessun provider configurato.
    NoProviders,
    /// Un provider ha risposto con un errore **del client**: si restituisce com'è.
    ClientFault {
        /// Chi ha risposto.
        provider: String,
        /// Lo stato.
        status: u16,
        /// Il corpo della risposta, per inoltrarlo.
        body: Vec<u8>,
    },
    /// Tutti i provider hanno risposto "non posso", o l'ultimo tentativo è finito
    /// così. **Non** è un errore del client e non è lo status di un provider
    /// singolo: è il fatto che il gateway, nel suo complesso, non ha potuto
    /// servire. Va detto come `502`, non inoltrando lo status del provider.
    ProviderUnavailable {
        /// L'ultimo provider che ha risposto.
        provider: String,
        /// Lo status che ha dato.
        status: u16,
    },
    /// Un provider ha risposto con uno status che il gateway non sa classificare.
    UnknownStatus {
        /// Chi ha risposto.
        provider: String,
        /// Lo stato.
        status: u16,
        /// Il corpo, per inoltrarlo.
        body: Vec<u8>,
    },
    /// La chiamata è fallita e la richiesta era già partita: **non** si ritenta.
    DeliveryUnknown {
        /// Il provider che non ha saputo.
        provider: String,
        /// L'errore.
        error: TransportError,
    },
    /// Tutti i tentativi sono finiti.
    Exhausted {
        /// Quanti tentativi sono stati fatti.
        attempts: usize,
        /// L'ultimo errore incontrato, per il messaggio al client.
        last: Box<RouteError>,
    },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProviderForModel { model, servibili } => write!(
                f,
                "nessun provider serve il modello {model:?}; disponibili: {}",
                if servibili.is_empty() { "(nessuno)".to_owned() } else { servibili.join(", ") }
            ),
            Self::NoProviders => f.write_str("nessun provider configurato"),
            Self::ClientFault { provider, status, .. } => {
                write!(f, "{provider} ha respinto la richiesta ({status}): errore del cliente, non si ritenta")
            }
            Self::ProviderUnavailable { provider, status, .. } => write!(
                f,
                "nessun provider ha potuto servire la richiesta (ultimo: {provider}, {status})"
            ),
            Self::UnknownStatus { provider, status, .. } => write!(
                f,
                "{provider} ha risposto {status}, uno stato che il gateway non sa classificare: non si ritenta"
            ),
            Self::DeliveryUnknown { provider, error } => write!(
                f,
                "{provider}: {error} — la richiesta era già partita, non si ritenta per evitare un doppio addebito"
            ),
            Self::Exhausted { attempts, last } => write!(f, "{attempts} tentativi esauriti ({last})"),
        }
    }
}

impl std::error::Error for RouteError {}

/// Una risposta servita, con il nome di chi l'ha servita.
#[derive(Debug)]
pub struct Routed {
    /// Chi ha risposto.
    pub provider: String,
    /// Quanti tentativi sono serviti prima.
    pub attempts: usize,
    /// La risposta.
    pub response: UpstreamResponse,
}
/// Il router.
#[derive(Clone)]
pub struct Router {
    providers: Vec<SharedUpstream>,
    max_attempts: usize,
    timeout: Duration,
}

/// `Debug` a mano: i provider sono trait object, e un `{:?}` che ne stampasse le
/// internals finirebbe dentro un log. Qui si vede **quali** provider e con che
/// tetto, che è la domanda che si fa durante un incidente.
impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field(
                "providers",
                &self.providers.iter().map(|p| p.name()).collect::<Vec<_>>(),
            )
            .field("max_attempts", &self.max_attempts)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Router {
    /// Crea un router. `max_attempts` a zero diventa [`DEFAULT_MAX_ATTEMPTS`]:
    /// un tetto di zero non è "nessun tentativo", è un gateway che non serve nessuno.
    #[must_use]
    pub fn new(providers: Vec<SharedUpstream>, max_attempts: usize, timeout: Duration) -> Self {
        Self {
            providers,
            max_attempts: if max_attempts == 0 {
                DEFAULT_MAX_ATTEMPTS
            } else {
                max_attempts
            },
            timeout,
        }
    }

    /// I provider, in ordine di preferenza.
    #[must_use]
    pub fn providers(&self) -> &[SharedUpstream] {
        &self.providers
    }

    /// Il tetto di tentativi.
    #[must_use]
    pub fn max_attempts(&self) -> usize {
        self.max_attempts
    }

    /// I modelli dichiarati da qualche provider, per un messaggio di errore utile.
    ///
    /// Un provider che serve **tutti** i modelli non compare: nell'errore "nessuno
    /// serve questo modello" ciò che serve è la lista di quelli **non** disponibili,
    /// e un "tutti i modelli" non aggiunge niente.
    #[must_use]
    pub fn servable_models(&self) -> Vec<String> {
        let mut tutti: Vec<String> = self
            .providers
            .iter()
            .filter_map(|p| p.models())
            .flatten()
            .cloned()
            .collect();
        tutti.sort();
        tutti.dedup();
        tutti
    }

    /// Invia la richiesta, con failover.
    pub async fn route(&self, request: UpstreamRequest) -> Result<Routed, RouteError> {
        if self.providers.is_empty() {
            return Err(RouteError::NoProviders);
        }

        let candidati: Vec<&SharedUpstream> = self
            .providers
            .iter()
            .filter(|p| p.supports(&request.model))
            .collect();

        if candidati.is_empty() {
            return Err(RouteError::NoProviderForModel {
                model: request.model.clone(),
                servibili: self.servable_models(),
            });
        }

        let mut tentativi = 0usize;
        let mut ultimo: Option<RouteError> = None;

        for provider in candidati {
            if tentativi >= self.max_attempts {
                debug!(attempts = tentativi, "tetto di tentativi raggiunto");
                break;
            }

            tentativi += 1;
            match self.attempt(provider.as_ref(), &request).await {
                Attempt::Served(response) => {
                    return Ok(Routed {
                        provider: provider.name().to_owned(),
                        attempts: tentativi,
                        response,
                    });
                }
                Attempt::Proseguire(errore) => ultimo = Some(errore),
                Attempt::Fermarsi(errore) => return Err(errore),
            }
        }

        Err(match ultimo {
            Some(last) if tentativi >= self.max_attempts => RouteError::Exhausted {
                attempts: tentativi,
                last: Box::new(last),
            },
            Some(last) => last,
            None => RouteError::Exhausted {
                attempts: tentativi,
                last: Box::new(RouteError::NoProviders),
            },
        })
    }

    /// Un tentativo su un provider, e cosa comporta per i successivi.
    ///
    /// È qui che sta tutta la politica di ADR 0003, ed è una funzione a sé perché a
    /// leggerla intera la differenza fra "proseguire" e "fermarsi" è evidente.
    async fn attempt(&self, provider: &dyn Upstream, request: &UpstreamRequest) -> Attempt {
        let nome = provider.name();

        let risposta = match provider.send(request.clone(), self.timeout).await {
            Ok(r) => r,
            Err(errore) => return on_transport(nome, errore),
        };

        if (200..300).contains(&risposta.status) {
            return Attempt::Served(risposta);
        }

        match classify(risposta.status) {
            FailureClass::Retryable => {
                warn!(
                    provider = nome,
                    status = risposta.status,
                    "errore del provider, si prosegue"
                );
                Attempt::Proseguire(classify_response(nome, risposta.status, body_of(&risposta)))
            }
            FailureClass::ClientFault => {
                info!(
                    provider = nome,
                    status = risposta.status,
                    "errore del cliente: nessun failover"
                );
                Attempt::Fermarsi(classify_response(nome, risposta.status, body_of(&risposta)))
            }
            FailureClass::Unknown => {
                // amplificare un errore che non si capisce è peggio che propagarlo
                warn!(
                    provider = nome,
                    status = risposta.status,
                    "stato non classificato: nessun failover su un errore non capito"
                );
                Attempt::Fermarsi(classify_response(nome, risposta.status, body_of(&risposta)))
            }
        }
    }
}

/// L'esito di un tentativo, e cosa comporta per il tentativo successivo.
#[derive(Debug)]
enum Attempt {
    /// Risposta buona: il giro è finito.
    Served(UpstreamResponse),
    /// Errore del provider, o richiesta mai partita: si prova il successivo.
    Proseguire(RouteError),
    /// Errore del cliente, stato ignoto, o richiesta già partita: ci si ferma.
    Fermarsi(RouteError),
}

/// Cosa fare quando un provider non ha risposto.
///
/// La domanda non è "che errore è" ma **"la richiesta è uscita?"**: se non è uscita
/// si prosegue, se è partita e non sappiamo se è stata eseguita no — un doppio
/// addebito costa più di un errore (ADR 0003).
fn on_transport(provider: &str, errore: TransportError) -> Attempt {
    if errore.delivery == Delivery::NotSent {
        warn!(
            provider,
            kind = ?errore.kind,
            detail = %errore.detail,
            "nessuna risposta, la richiesta non era uscita: si prosegue"
        );
        return Attempt::Proseguire(RouteError::DeliveryUnknown {
            provider: provider.to_owned(),
            error: errore,
        });
    }

    warn!(
        provider,
        kind = ?errore.kind,
        detail = %errore.detail,
        "la richiesta era già partita: nessun failover per evitare un doppio addebito"
    );
    Attempt::Fermarsi(RouteError::DeliveryUnknown {
        provider: provider.to_owned(),
        error: errore,
    })
}

/// Trasforma una risposta d'errore nel suo `RouteError`, senza perderne la classe.
fn classify_response(provider: &str, status: u16, body: Vec<u8>) -> RouteError {
    match classify(status) {
        FailureClass::ClientFault => RouteError::ClientFault {
            provider: provider.to_owned(),
            status,
            body,
        },
        FailureClass::Retryable => RouteError::ProviderUnavailable {
            provider: provider.to_owned(),
            status,
        },
        FailureClass::Unknown => RouteError::UnknownStatus {
            provider: provider.to_owned(),
            status,
            body,
        },
    }
}

/// Il corpo di una risposta, se è in memoria. Su uno streaming non c'è: ed è il
/// motivo per cui il metering di uno streaming va fatto a valle, dal client.
fn body_of(response: &UpstreamResponse) -> Vec<u8> {
    match &response.body {
        ResponseBody::Buffered(b) => b.clone(),
        ResponseBody::Stream(_) => Vec::new(),
    }
}

/// Costruisce l'errore "la richiesta non è uscita", per chi implementa un provider e
/// non vuole costruire l'errore a mano.
#[must_use]
pub fn errore_non_inviato(kind: TransportKind, detail: &str) -> TransportError {
    TransportError::not_sent(kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_router_vuoto_ha_zero_provider() {
        let r = Router::new(vec![], 4, Duration::from_secs(1));
        assert_eq!(r.max_attempts(), 4);
        assert!(r.providers().is_empty());
    }

    #[test]
    fn un_tetto_di_zero_diventa_il_default() {
        let r = Router::new(vec![], 0, Duration::from_secs(1));
        assert_eq!(r.max_attempts(), DEFAULT_MAX_ATTEMPTS);
    }

    #[test]
    fn l_errore_di_trasporto_si_stampa_in_una_riga() {
        let e = RouteError::DeliveryUnknown {
            provider: "a".to_owned(),
            error: TransportError::maybe_sent(TransportKind::Timeout, "timeout dopo l'invio"),
        };
        let testo = e.to_string();
        assert!(testo.contains("doppio addebito"));
        assert!(
            !testo.contains('\n'),
            "un messaggio di log non deve andare a capo"
        );
    }

    #[test]
    fn un_modello_che_nessuno_serve_lo_dice_con_lelenco_vuoto() {
        let e = RouteError::NoProviderForModel {
            model: "x".to_owned(),
            servibili: vec![],
        };
        assert!(e.to_string().contains("(nessuno)"));
    }

    #[test]
    fn il_debug_del_router_mostra_i_provider_e_il_tetto() {
        let r = Router::new(vec![], 7, Duration::from_millis(250));
        let testo = format!("{r:?}");
        assert!(testo.contains("max_attempts: 7"));
        assert!(testo.contains("250"));
    }
}
