//! La richiesta in arrivo, vista dal gateway.
//!
//! Il gateway non capisce cosa contiene un prompt, e non deve. Deve però sapere tre
//! cose, e solo tre: **qual è il modello** (per il prezzo e per il routing),
//! **quanto output si chiede** (per stimare il costo prima di chiamare) e
//! **se è in streaming** (per non bufferizzare).
//!
//! Tutto il resto del corpo viene inoltrato **byte per byte**. Non è una
//! semplificazione: il gateway che riserializza il body di un provider finisce per
//! perdere i campi che non conosce, e un giorno un campo nuovo arriva e sparisce
//! senza che nessuno se ne accorga.
//!
//! I tipi sono estratti dal body JSON senza tipizzarlo tutto: un `serde_json::Value`
//! dell'intera richiesta costerebbe un parse completo per due stringhe.

use serde::Deserialize;

/// I byte di un body HTTP.
pub type Bytes = Vec<u8>;

/// Byte per token, la stima standard per i testi in inglese.
///
/// Per eccesso significa che **non si sottovaluta mai**: un testo italiano o del
/// codice usa più token per carattere, e il divisore va bene per un tetto, male per
/// un preventivo. Vedi ADR 0002.
pub const BYTES_PER_TOKEN: usize = 4;

/// Output massimo che si presume quando il client non lo dichiara.
///
/// Volutamente generoso: se il client non dice quanto vuole generare, si presume il
/// peggio. Sottovalutare qui significa che il tetto non copre.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4_096;

/// Quanto il gateway ha capito di una richiesta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestShape {
    /// Il modello richiesto. `None` se il body non lo dichiara in modo leggibile.
    pub model: Option<String>,
    /// I token di output massimi dichiarati dal client, se lo fa.
    pub max_output_tokens: Option<u64>,
    /// `true` se il client ha chiesto lo streaming.
    pub stream: bool,
    /// La stima dei token di input.
    pub estimated_input_tokens: u64,
    /// La lunghezza del body, che è ciò che si fa pagare in banda.
    pub body_bytes: usize,
}

/// I tre campi che il gateway deve leggere dal body.
///
/// `deny_unknown_fields` **non** c'è, e questa è la scelta: il body appartiene al
/// provider, non al gateway, e rifiutarlo perché ha un campo in più significherebbe
/// che il gateway deve essere aggiornato ogni volta che il provider ne aggiunge uno.
#[derive(Debug, Deserialize)]
struct ModelAndLimits {
    model: Option<String>,
    max_tokens: Option<u64>,
    stream: Option<bool>,
}

/// Legge dal body tutto ciò che serve al gateway, e **non solleva mai**.
///
/// Un body non è JSON, o è JSON senza `model`, non è un errore del gateway: è una
/// richiesta che il provider rifiuterà. Qui si estrae quello che si può, e si lascia
/// la diagnosi a chi sa rispondere (il provider, o il router se nessuno lo sa fare).
#[must_use]
pub fn inspect(body: &[u8], max_output_default: u64) -> RequestShape {
    let forma: Option<ModelAndLimits> = serde_json::from_slice(body).ok();

    RequestShape {
        model: forma.as_ref().and_then(|f| f.model.clone()),
        max_output_tokens: forma
            .as_ref()
            .and_then(|f| f.max_tokens)
            .filter(|n| *n > 0)
            .or(Some(max_output_default)),
        stream: forma.as_ref().and_then(|f| f.stream).unwrap_or(false),
        estimated_input_tokens: estimate_input_tokens(body.len()),
        body_bytes: body.len(),
    }
}

/// I token di input stimati dalla lunghezza del body.
///
/// Arrotondata **per eccesso**: una stima che sottovaluta è una stima che lascia
/// passare richieste costose, e il tetto smette di coprire.
#[must_use]
pub fn estimate_input_tokens(body_bytes: usize) -> u64 {
    body_bytes.div_ceil(BYTES_PER_TOKEN) as u64
}

impl RequestShape {
    /// Il tetto massimo che questa richiesta può costare, dato un prezzo.
    ///
    /// È la stima prenotata prima di chiamare: input stimato più output massimo,
    /// entrambi al prezzo indicato. `None` senza un modello: senza modello non c'è
    /// prezzo, e il tetto non può essere calcolato.
    #[must_use]
    pub fn worst_case_cost(
        &self,
        price: crate::pricing::Price,
    ) -> Option<crate::pricing::MicroUsd> {
        let model = self.model.as_deref()?;
        let _ = model;
        Some(price.cost(self.estimated_input_tokens, self.max_output_tokens?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legge_modello_e_output_massimo() {
        let body = br#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"ciao"}],"max_tokens":256}"#;
        let forma = inspect(body, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(forma.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(forma.max_output_tokens, Some(256));
        assert!(!forma.stream);
    }

    #[test]
    fn senza_max_tokens_si_presume_il_peggio() {
        // se il client non lo dichiara, si stima il massimo configurato: sottovalutare
        // qui significa che il tetto non copre
        let forma = inspect(br#"{"model":"m"}"#, 8192);
        assert_eq!(forma.max_output_tokens, Some(8192));
    }

    #[test]
    fn max_tokens_zero_vale_come_non_dichiarato() {
        // zero token di output non ha senso in una richiesta vera: è un bug del client,
        // e il tetto non deve coprirlo come se fosse una richiesta gratuita
        let forma = inspect(br#"{"model":"m","max_tokens":0}"#, 4096);
        assert_eq!(forma.max_output_tokens, Some(4096));
    }

    #[test]
    fn lo_streaming_si_riconosce() {
        assert!(inspect(br#"{"model":"m","stream":true}"#, 4096).stream);
        assert!(!inspect(br#"{"model":"m","stream":false}"#, 4096).stream);
    }

    #[test]
    fn i_campi_sconosciuti_non_sono_un_problema() {
        // il body è del provider: un campo nuovo non può rendere la richiesta
        // illeggibile per il gateway
        let corpo = br#"{"model":"m","temperature":0.7,"tools":[{"type":"function"}],"reasoning_effort":"high"}"#;
        assert_eq!(inspect(corpo, 4096).model.as_deref(), Some("m"));
    }

    #[test]
    fn un_body_illeggibile_non_fa_panorama_e_da_modello_sconosciuto() {
        // non è un errore del gateway: è una richiesta che il provider rifiuterà
        let forma = inspect("non è json".as_bytes(), 4096);
        assert_eq!(forma.model, None);
        assert_eq!(forma.max_output_tokens, Some(4096));
        assert!(!forma.stream);
        assert_eq!(
            forma.estimated_input_tokens, 3,
            "11 byte arrotondati per eccesso: 3 token"
        );
    }

    #[test]
    fn la_stima_dei_token_arrotonda_per_eccesso() {
        assert_eq!(estimate_input_tokens(0), 0);
        assert_eq!(estimate_input_tokens(1), 1);
        assert_eq!(estimate_input_tokens(4), 1);
        assert_eq!(
            estimate_input_tokens(5),
            2,
            "mai sotto: 5 byte sono almeno 2 token"
        );
        assert_eq!(estimate_input_tokens(401), 101);
    }

    #[test]
    fn il_costo_peggiore_usa_input_stimato_e_output_massimo() {
        let body = br#"{"model":"m","max_tokens":1000}"#;
        let forma = inspect(body, 4096);
        let prezzo = crate::pricing::Price {
            input: 150,
            output: 600,
        };

        // il body è lungo: la stima dell'input deve entrare nel conto
        let atteso = prezzo.cost(forma.estimated_input_tokens, 1000);
        assert_eq!(forma.worst_case_cost(prezzo), Some(atteso));
    }

    #[test]
    fn senza_modello_il_costo_peggiore_non_si_calcola() {
        let forma = inspect(b"rotta", 4096);
        assert_eq!(forma.worst_case_cost(crate::pricing::Price::ZERO), None);
    }

    #[test]
    fn un_body_vuoto_non_e_un_panic() {
        let forma = inspect(b"", 4096);
        assert_eq!(forma.estimated_input_tokens, 0);
        assert_eq!(forma.body_bytes, 0);
    }
}
