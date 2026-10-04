// Questo modulo è compilato due volte, una per ogni binario di test che lo include.
// Ogni copia vede solo gli strumenti che il proprio test usa, e segnalerebbe gli
// altri come morti. È un toolbox condiviso: la metà inutilizzata è normale.
#![allow(dead_code)]

//! Un provider finto, per testare il router senza rete.
//!
//! È qui che il progetto guadagna da `Upstream` essere un trait: l'intera politica di
//! failover — le tre classi di errore, il tetto di tentativi, il divieto di
//! ritentare una richiesta già partita — si verifica in microsecondi e senza che un
//! provider di mezzo abbia cambiato risposta, che è il problema dei test che
//! colpiscono un'API vera.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use llmgateway::upstream::{
    BoxFuture, TransportError, TransportKind, Upstream, UpstreamRequest, UpstreamResponse,
};

/// Cosa deve fare il provider finto.
#[derive(Debug, Clone)]
pub enum Comportamento {
    /// Risponde con questo status e questo corpo.
    Risponde(u16, String),
    /// Non produce risposta, e la richiesta **non è uscita**.
    NonEsce(TransportKind),
    /// Non produce risposta, e la richiesta **è partita**.
    PartitaSenzaRisposta(TransportKind),
}

impl Comportamento {
    /// Risponde `200` con un corpo JSON.
    #[must_use]
    pub fn ok(corpo: &str) -> Self {
        Self::Risponde(200, corpo.to_owned())
    }

    /// Risponde `503`.
    #[must_use]
    pub fn non_disponibile() -> Self {
        Self::Risponde(503, "{\"error\":\"service unavailable\"}".to_owned())
    }
}

/// Un provider che fa esattamente quello che gli si dice, e conta quante volte.
#[derive(Debug)]
pub struct Finto {
    nome: String,
    modelli: Option<Vec<String>>,
    comportamenti: Mutex<Vec<Comportamento>>,
    chiamate: AtomicUsize,
    ultimi_modelli: Mutex<Vec<String>>,
}

impl Finto {
    /// Un provider che risponde sempre `200` a qualunque modello.
    #[must_use]
    pub fn nuovo(nome: &str) -> Self {
        Self {
            nome: nome.to_owned(),
            modelli: None,
            comportamenti: Mutex::new(vec![Comportamento::ok("{\"ok\":true}")]),
            chiamate: AtomicUsize::new(0),
            ultimi_modelli: Mutex::new(Vec::new()),
        }
    }

    /// Un provider che serve solo questi modelli.
    #[must_use]
    pub fn con_modelli(nome: &str, modelli: &[&str]) -> Self {
        let mut finto = Self::nuovo(nome);
        finto.modelli = Some(modelli.iter().map(|m| (*m).to_owned()).collect());
        finto
    }

    /// Imposta gli atteggiamenti, uno per tentativo. L'ultimo vale per tutti i
    /// tentativi successivi: un provider che risponde sempre `503` si scrive
    /// con un solo elemento.
    #[must_use]
    pub fn con_comportamenti(self, comportamenti: Vec<Comportamento>) -> Self {
        *self.comportamenti.lock().expect("lock dei comportamenti") = comportamenti;
        self
    }

    /// Quante volte è stato chiamato.
    #[must_use]
    pub fn chiamate(&self) -> usize {
        self.chiamate.load(Ordering::SeqCst)
    }

    /// I modelli che gli sono stati chiesti, in ordine.
    #[must_use]
    pub fn modelli_richiesti(&self) -> Vec<String> {
        self.ultimi_modelli
            .lock()
            .expect("lock dei modelli")
            .clone()
    }

    fn prossimo(&self) -> Comportamento {
        let mut lista = self.comportamenti.lock().expect("lock dei comportamenti");
        if lista.len() == 1 {
            return lista[0].clone();
        }
        lista.remove(0)
    }
}

impl Upstream for Finto {
    fn name(&self) -> &str {
        &self.nome
    }

    fn models(&self) -> Option<&[String]> {
        self.modelli.as_deref()
    }

    fn send(
        &self,
        request: UpstreamRequest,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<UpstreamResponse, TransportError>> {
        self.chiamate.fetch_add(1, Ordering::SeqCst);
        self.ultimi_modelli
            .lock()
            .expect("lock dei modelli")
            .push(request.model.clone());

        // tutto ciò che il future usa viene **copiato**: il future vive più a lungo
        // del prestito di `&self`, e tenere il riferimento qui dentro non compilerebbe
        let comportamento = self.prossimo();
        let nome = self.nome.clone();
        let stream = request.stream;

        llmgateway::upstream::boxed(move || async move {
            let nome = nome.as_str();
            match comportamento {
                Comportamento::Risponde(200, corpo) if stream => {
                    // lo streaming arriva a pezzi: il gateway deve inoltrarlo senza
                    // aspettare l'ultimo
                    let chunk: Vec<Vec<u8>> = corpo
                        .lines()
                        .map(|l| format!("data: {l}\n\n").into_bytes())
                        .collect();
                    Ok(UpstreamResponse::streaming(200, chunk))
                }
                Comportamento::Risponde(status, corpo) => {
                    Ok(UpstreamResponse::buffered(status, corpo))
                }
                Comportamento::NonEsce(kind) => Err(TransportError::not_sent(
                    kind,
                    format!("{nome} non raggiungibile"),
                )),
                Comportamento::PartitaSenzaRisposta(kind) => Err(TransportError::maybe_sent(
                    kind,
                    format!("{nome} ha spento dopo aver ricevuto la richiesta"),
                )),
            }
        })
    }
}

/// Un provider condiviso, pronto per il router.
#[must_use]
pub fn condiviso(finto: Finto) -> Arc<dyn Upstream> {
    Arc::new(finto)
}
