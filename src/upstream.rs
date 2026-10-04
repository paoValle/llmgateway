//! L'astrazione sul provider: cosa sa fare un upstream, e cosa può sbagliare.
//!
//! `Upstream` è un trait, e lo è per una ragione che non è la testabilità (che è
//! un effetto collaterale): se il provider è un'interfaccia, il **failover è logica di
//! dominio**, non codice HTTP. La classificazione degli errori in ADR 0003, la
//! prenotazione del budget, il tetto sugli tentativi: tutto questo sta sopra
//! l'interfaccia e non sa nulla di `reqwest`.
//!
//! Il punto più delicato del file è [`Delivery`]. Quando una chiamata fallisce, la
//! domanda che conta non è "che errore è" ma **"la richiesta è uscita?"**:
//!
//! - se non è uscita, ritentare su un altro provider è sicuro: nessuno l'ha eseguita;
//! - se è uscita e non sappiamo se è stata eseguita, ritentare può **facciare due
//!   volte** la stessa operazione. Meglio un errore che un doppio addebito.
//!
//! Un timeout e una connessione refused sembrano lo stesso errore e non lo sono.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::Stream;

use crate::request::Bytes;

/// Un future che si può mettere dietro un `dyn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Un flusso di chunk: lo streaming SSE del provider, senza bufferizzarlo.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send>>;

/// Una richiesta verso un provider.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    /// Il body così com'è, byte per byte.
    pub body: Bytes,
    /// Il modello richiesto, per il routing.
    pub model: String,
    /// `true` se il client ha chiesto lo streaming.
    pub stream: bool,
}

impl UpstreamRequest {
    /// Costruisce una richiesta dalla forma letta dal body in arrivo.
    #[must_use]
    pub fn new(body: Bytes, model: impl Into<String>, stream: bool) -> Self {
        Self {
            body,
            model: model.into(),
            stream,
        }
    }
}

/// Il corpo di una risposta: intero, o in corso.
pub enum ResponseBody {
    /// Tutto in memoria. La via normale.
    Buffered(Bytes),
    /// Ancora in corso. Il gateway lo inoltra senza accumularlo: accumulare uno
    /// streaming significa far aspettare il primo token finché non arriva l'ultimo,
    /// cioè togliere al client l'unica cosa per cui lo streaming esiste.
    Stream(ByteStream),
}

/// `Debug` scritto a mano: il payload di uno streaming non è formattabile, e un
/// `{:?}` che stampa i chunk interi finirebbe in un log con dentro la risposta del
/// provider. Qui si stampa **che forma ha** il corpo, non il suo contenuto — che è
/// ciò che serve per capire un errore.
impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buffered(b) => f
                .debug_tuple("Buffered")
                .field(&format_args!("{} byte", b.len()))
                .finish(),
            Self::Stream(_) => f.write_str("Stream(<in corso>)"),
        }
    }
}

impl ResponseBody {
    /// `true` se è uno streaming.
    #[must_use]
    pub fn is_stream(&self) -> bool {
        matches!(self, Self::Stream(_))
    }
}

/// Una risposta che è arrivata. Può comunque essere un errore.
#[derive(Debug)]
pub struct UpstreamResponse {
    /// Lo stato HTTP.
    pub status: u16,
    /// Il corpo.
    pub body: ResponseBody,
}

impl UpstreamResponse {
    /// Una risposta in memoria, comoda per i test e per i provider che non fanno streaming.
    #[must_use]
    pub fn buffered(status: u16, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            body: ResponseBody::Buffered(body.into()),
        }
    }

    /// Una risposta in streaming.
    #[must_use]
    pub fn streaming(status: u16, chunks: Vec<Bytes>) -> Self {
        Self {
            status,
            body: ResponseBody::Stream(Box::pin(futures_util::stream::iter(
                chunks.into_iter().map(Ok),
            ))),
        }
    }
}

/// Se la richiesta è uscita, e che cosa si può fare.
///
/// È la distinzione che separa "ritenta" da "non ritentare" (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// La richiesta **non è arrivata** al provider: connessione rifiutata, DNS
    /// fallito, rotta assente. Riprovare altrove è sicuro.
    NotSent,
    /// La richiesta **è partita** e non si sa se è stata eseguita: timeout dopo
    /// l'invio, connessione caduta a metà. Riprovare può costare due volte.
    Unknown,
}

impl Delivery {
    /// `true` se si può passare a un altro provider senza rischio di doppio addebito.
    #[must_use]
    pub fn safe_to_retry(self) -> bool {
        matches!(self, Self::NotSent)
    }
}

/// Perché una chiamata non ha prodotto una risposta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Connessione rifiutata, DNS, rotta assente: non è uscito nulla.
    Connect,
    /// Timeout.
    Timeout,
    /// Connessione caduta dopo l'invio.
    Reset,
    /// TLS, DNS e tutto il resto che non è né connessione né timeout.
    Other,
}

/// Un errore di trasporto: nessuna risposta è arrivata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// Di che natura è.
    pub kind: TransportKind,
    /// Se la richiesta è uscita. **Non si può dedurre dal tipo di errore.**
    pub delivery: Delivery,
    /// Una riga descrittiva, per i log. Non finisce mai in una risposta al client.
    pub detail: String,
}

impl TransportError {
    /// Un errore in cui la richiesta non è uscita: si può ritentare altrove.
    #[must_use]
    pub fn not_sent(kind: TransportKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            delivery: Delivery::NotSent,
            detail: detail.into(),
        }
    }

    /// Un errore in cui la richiesta è partita e non si sa: **non** si ritenta.
    #[must_use]
    pub fn maybe_sent(kind: TransportKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            delivery: Delivery::Unknown,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} ({:?}): {}", self.kind, self.delivery, self.detail)
    }
}

impl std::error::Error for TransportError {}

/// A chi è stata la colpa di una risposta che è un errore.
///
/// Tre classi, e la terza è quella che si dimentica (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Del provider: si passa al successivo.
    Retryable,
    /// Del client: non si ritenta, si restituisce. Un `400` rilanciato su un altro
    /// provider diventa un `400` dopo una latenza che l'utente ha già visto.
    ClientFault,
    /// Non sappiamo cosa sia. **Non si ritenta** e si segnala: un errore non capito
    /// amplificato è peggio di un errore propagato.
    Unknown,
}

/// Classifica uno status HTTP.
///
/// La regola è corta da ricordare: **4xx è del client, 5xx è del provider**, e
/// `429` sta dalla parte del provider anche se è un 4xx, perché è un rate limit e il
/// provider successivo è esattamente la risposta giusta.
#[must_use]
pub fn classify(status: u16) -> FailureClass {
    match status {
        // 408 e 429 sono del provider anche se sono 4xx: sono timeout e rate limit,
        // e il provider successivo è esattamente la risposta giusta
        408 | 429 | 500..=599 => FailureClass::Retryable,
        400..=499 => FailureClass::ClientFault,
        _ => FailureClass::Unknown,
    }
}

/// Un provider.
///
/// Object-safe a mano (`BoxFuture`) invece che con `async_trait`: è la stessa cosa
/// senza una dipendenza, e ADR 0001 tiene il grafo chiuso.
pub trait Upstream: Send + Sync {
    /// Il nome con cui compare nei log e nelle metriche.
    fn name(&self) -> &str;

    /// `true` se questo provider serve il modello richiesto.
    ///
    /// Un provider che non lo serve viene **saltato**, non trattato come fallito:
    /// non è un errore, è una scelta di routing, e contarlo come errore
    /// farebbe urlare allarme ogni volta che si cambia modello.
    /// I modelli che questo provider serve. `None` significa **tutti**: molti
    /// provider rispondono a qualunque modello, e obbligarli a elencarli sarebbe una
    /// lista da mantenere che invecchia male.
    fn models(&self) -> Option<&[String]>;

    /// `true` se questo provider serve il modello richiesto.
    ///
    /// Un provider che non lo serve viene **saltato**, non trattato come fallito: non
    /// è un errore, è una scelta di routing, e contarlo come errore farebbe urlare
    /// allarme ogni volta che si cambia modello.
    fn supports(&self, model: &str) -> bool {
        match self.models() {
            None => true,
            Some(modelli) => modelli.iter().any(|m| m == model),
        }
    }

    /// Invia la richiesta. Non deve ritentare da solo: il tentativo è una decisione
    /// del router, che conosce budget e tetto.
    fn send(
        &self,
        request: UpstreamRequest,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<UpstreamResponse, TransportError>>;
}

/// Come costruire un `Upstream` a mano, senza scrivere a mano il `Box::pin`.
pub fn boxed<F, Fut, T>(fut: F) -> BoxFuture<'static, T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    Box::pin(fut())
}

/// Un provider condiviso.
pub type SharedUpstream = Arc<dyn Upstream>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_4xx_è_del_cliente_e_un_5xx_del_provider() {
        assert_eq!(classify(400), FailureClass::ClientFault);
        assert_eq!(classify(404), FailureClass::ClientFault);
        assert_eq!(classify(422), FailureClass::ClientFault);
        assert_eq!(classify(500), FailureClass::Retryable);
        assert_eq!(classify(503), FailureClass::Retryable);
    }

    #[test]
    fn il_429_è_del_provider_anche_se_è_un_4xx() {
        // è un rate limit: il provider successivo è la risposta giusta
        assert_eq!(classify(429), FailureClass::Retryable);
        assert_eq!(classify(408), FailureClass::Retryable);
    }

    #[test]
    fn uno_status_che_non_è_del_client_né_del_provider_non_è_rientrabile() {
        // un 3xx che il gateway non instrada, un 1xx, un 6xx: il gateway non sa
        // cosa siano, e non fare niente è più onesto che amplificarli su tre provider
        for s in [100, 101, 301, 302, 600, 999] {
            assert_eq!(classify(s), FailureClass::Unknown, "status {s}");
        }
    }

    #[test]
    fn un_4xx_che_non_capiamo_ferma_il_failover_come_ogni_altro_errore_del_cliente() {
        // 402 e 451 non sono errori che si risolvono riprovando: il cliente non
        // paga e la richiesta è illecita. Non sono "status ignoti", sono del
        // cliente come un 400 — e il router si ferma in entrambi i casi
        for s in [402, 451, 413, 422] {
            assert_eq!(classify(s), FailureClass::ClientFault, "status {s}");
        }
    }

    #[test]
    fn solo_ciò_che_non_è_uscito_si_ritenta() {
        assert!(Delivery::NotSent.safe_to_retry());
        assert!(!Delivery::Unknown.safe_to_retry());
    }

    #[test]
    fn un_timeout_dopo_linvio_non_e_sicuro_come_un_rifiuto_di_connessione() {
        // lo stesso sintomo visibile, due fatti diversi
        let rifiutata = TransportError::not_sent(TransportKind::Connect, "connection refused");
        let scaduto = TransportError::maybe_sent(TransportKind::Timeout, "timeout");
        assert!(rifiutata.delivery.safe_to_retry());
        assert!(!scaduto.delivery.safe_to_retry());
    }

    #[test]
    fn il_corpo_di_una_risposta_sa_dire_se_streaming() {
        assert!(!UpstreamResponse::buffered(200, "{}").body.is_stream());
        assert!(UpstreamResponse::streaming(200, vec![]).body.is_stream());
    }

    #[test]
    fn l_errore_di_trasporto_si_stampa_senza_fuoco_sui_dettagli() {
        let e = TransportError::maybe_sent(TransportKind::Reset, "connessione caduta");
        assert!(e.to_string().contains("Reset"));
        assert!(e.to_string().contains("connessione caduta"));
    }
}
