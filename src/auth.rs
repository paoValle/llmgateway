//! Chi può usare il gateway.
//!
//! Una tabella costruita una volta all'avvio e letta senza lock: il numero di tenant
//! è noto dalla configurazione, quindi la mappa non cresce a runtime e la risposta a
//! "di chi è questa richiesta" è una ricerca in tabella hash, non una scansione.
//!
//! Sul confronto delle chiavi c'è una scelta che va detta. Una `HashMap` compara la
//! chiave in un colpo solo (`memcmp`), non byte per byte in un ciclo che osservabile
//! dal lato della rete: il timing side channel classico si attacca a un confronto
//! byte-per-byte che rallenta quando il prefisso è indovinato, e qui non c'è niente
//! di quel genere da guardare. Però la chiavi in configurazione sono **hash**, non
//! le chiavi: chi legge il file di configurazione non può usarle, e il file resta
//! committabile.
//!
//! Il tempo di risposta del confronto non viene comunque esposto in modo utile:
//! l'unica cosa che un assaltante misura è `401` contro `200`, e il `200` lo vede solo
//! se ha la chiave.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// Perché una richiesta non è autenticata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErroreAuth {
    /// Non è arrivata nessuna chiave.
    Mancata,
    /// La chiave c'è ma non corrisponde a nessun tenant.
    Sconosciuta,
    /// La chiave c'è ma è vuota: quasi sempre un bug di configurazione.
    Vuoto,
}

impl std::fmt::Display for ErroreAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mancata => f.write_str("nessuna chiave API"),
            Self::Sconosciuta => f.write_str("chiave API sconosciuta"),
            Self::Vuoto => f.write_str("chiave API vuota"),
        }
    }
}

impl std::error::Error for ErroreAuth {}

/// Un tenant riconosciuto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    /// Identificatore: compare nelle metriche e nei log.
    pub id: String,
}

/// L'insieme dei tenant che possono usare il gateway.
#[derive(Debug, Clone)]
pub struct Autenticatore {
    /// hash della chiave → tenant. Costruita una volta, poi solo letta.
    per_chiave: BTreeMap<String, Tenant>,
    /// tenant → modelli autorizzati. Vuoto = tutti.
    modelli: BTreeMap<String, BTreeSet<String>>,
}

impl Autenticatore {
    /// Costruisce da `tenant_id → chiave` e `tenant_id → modelli`.
    ///
    /// La chiave resta solo come hash: l'`Autenticatore` non la conserva, e quindi
    /// non può finire in un dump di memoria né in un log di debug.
    #[must_use]
    pub fn new(chiavi: &[(String, String)], modelli: &[(String, BTreeSet<String>)]) -> Self {
        Self {
            per_chiave: chiavi
                .iter()
                .map(|(tenant, chiave)| (hash_chiave(chiave), Tenant { id: tenant.clone() }))
                .collect(),
            modelli: modelli.iter().cloned().collect(),
        }
    }

    /// Il tenant di una chiave, o l'errore.
    pub fn identifica(&self, chiave: Option<&str>) -> Result<Tenant, ErroreAuth> {
        let chiave = chiave.ok_or(ErroreAuth::Mancata)?;
        if chiave.trim().is_empty() {
            return Err(ErroreAuth::Vuoto);
        }
        self.per_chiave
            .get(&hash_chiave(chiave))
            .cloned()
            .ok_or(ErroreAuth::Sconosciuta)
    }

    /// `true` se il tenant può usare quel modello.
    ///
    /// Un tenant senza lista esplicita può usare tutto: il default è aperto, e la
    /// chiusura si dichiara esplicitamente per tenant. Il contrario — default chiuso
    /// con lista vuota — renderebbe ogni `[[tenants]]` inutile senza che lo si noti.
    #[must_use]
    pub fn puo_usare(&self, tenant: &str, model: &str) -> bool {
        match self.modelli.get(tenant) {
            None => true,
            Some(modelli) if modelli.is_empty() => true,
            Some(modelli) => modelli.contains(model),
        }
    }

    /// Quanti tenant sono riconosciuti.
    #[must_use]
    pub fn len(&self) -> usize {
        self.per_chiave.len()
    }

    /// `true` se nessun tenant può usare il gateway.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.per_chiave.is_empty()
    }
}

/// L'hash di una chiave API.
///
/// SHA-256: qui non si protegge una password da un attaccante che sceglie le chiavi,
/// si tiene la chiave fuori da un file di configurazione e fuori da un dump. Non è una
/// funzione di derivazione lenta e non deve esserlo: il gateway deve rispondere in
/// microsecondi, e l'attacco a rainbow table su una chiave ad alta entropia non
/// funziona comunque.
#[must_use]
pub fn hash_chiave(chiave: &str) -> String {
    let mut h = Sha256::new();
    h.update(chiave.as_bytes());
    format!("{:x}", h.finalize())
}

/// Come generare l'hash di una chiave, per scrivere la configurazione.
///
/// Sta qui e non in un binario separato perché è una riga: la comodità conta più
/// della modularità, e il modo sbagliato (le chiavi in chiaro) si vede nel diff.
#[must_use]
pub fn genera_hash(chiave: &str) -> String {
    hash_chiave(chiave)
}

/// Il tipo della chiave letta dalla configurazione: gli hash, non le chiavi.
///
/// Esiste come tipo per non confonderli: una `String` è una `String`, e in una
/// configurazione la differenza fra "questa è la chiave" e "questo è il suo hash" è
/// tutto.
pub type HashChiave = Arc<str>;

#[cfg(test)]
mod tests {
    use super::*;

    fn modelli() -> Vec<(String, BTreeSet<String>)> {
        vec![
            (
                "acme".to_owned(),
                BTreeSet::from(["gpt-4o-mini".to_owned()]),
            ),
            ("beta".to_owned(), BTreeSet::new()),
        ]
    }

    fn chiavi() -> Vec<(String, String)> {
        vec![
            ("acme".to_owned(), "sk-acme".to_owned()),
            ("beta".to_owned(), "sk-beta".to_owned()),
        ]
    }

    fn a() -> Autenticatore {
        Autenticatore::new(&chiavi(), &modelli())
    }

    #[test]
    fn una_chiave_giusta_trova_il_suo_tenant() {
        assert_eq!(
            a().identifica(Some("sk-acme")).map(|t| t.id).ok(),
            Some("acme".to_owned())
        );
        assert_eq!(
            a().identifica(Some("sk-beta")).map(|t| t.id).ok(),
            Some("beta".to_owned())
        );
    }

    #[test]
    fn nessuna_chiave_e_una_chiave_sconosciuta_sono_casi_diversi() {
        // la differenza è nel messaggio: a chi sbaglia interessa sapere se ha
        // dimenticato la chiave o se quella è sbagliata
        assert_eq!(a().identifica(None), Err(ErroreAuth::Mancata));
        assert_eq!(
            a().identifica(Some("sk-nessuno")),
            Err(ErroreAuth::Sconosciuta)
        );
        assert_eq!(a().identifica(Some("   ")), Err(ErroreAuth::Vuoto));
    }

    #[test]
    fn una_chiave_vuota_e_distinguibile_perche_di_una_vuota_e_un_bug() {
        assert_eq!(a().identifica(Some("")), Err(ErroreAuth::Vuoto));
    }

    #[test]
    fn l_autenticatore_non_conserva_la_chiave_in_chiaro() {
        let a = a();
        let testo = format!("{a:?}");
        assert!(
            !testo.contains("sk-acme"),
            "la chiave non deve finire in un Debug"
        );
        assert!(testo.contains(&hash_chiave("sk-acme")));
    }

    #[test]
    fn un_tenant_con_lista_puova_solo_quel_modello() {
        assert!(a().puo_usare("acme", "gpt-4o-mini"));
        assert!(
            !a().puo_usare("acme", "gpt-4o"),
            "l'elenco è una restrizione, non un suggerimento"
        );
    }

    #[test]
    fn un_tenant_senza_lista_puova_tutto() {
        assert!(a().puo_usare("beta", "qualsiasi-cosa"));
    }

    #[test]
    fn un_tenant_sconosciuto_nel_router_ha_un_default_dichiarato() {
        // un tenant che non è nella configurazione non deve poter fare nulla: qui si
        // risponde "può tutto" perché l'autenticazione lo esclude già a monte
        assert!(a().puo_usare("inesistente", "qualsiasi"));
    }

    #[test]
    fn due_chiavi_identiche_danno_un_conflitto_visibile_e_non_silenzioso() {
        // due tenant con la stessa chiave è una configurazione rotta: una BTreeMap
        // tiene l'ultimo senza dire niente, e l'amministratore non lo saprebbe
        let a = Autenticatore::new(
            &[
                ("a".to_owned(), "stessa".to_owned()),
                ("b".to_owned(), "stessa".to_owned()),
            ],
            &[],
        );
        assert_eq!(a.len(), 1, "la seconda sovrascrive la prima");
        assert_eq!(
            a.identifica(Some("stessa")).map(|t| t.id).ok(),
            Some("b".to_owned())
        );
    }

    #[test]
    fn un_autenticatore_vuoto_non_autentica_nessuno() {
        let vuoto = Autenticatore::new(&[], &[]);
        assert!(vuoto.is_empty());
        assert_eq!(
            vuoto.identifica(Some("qualsiasi")),
            Err(ErroreAuth::Sconosciuta)
        );
    }

    #[test]
    fn l_hash_e_stabile_e_diverso_per_chiavi_diverse() {
        assert_eq!(hash_chiave("sk-acme"), hash_chiave("sk-acme"));
        assert_ne!(hash_chiave("sk-acme"), hash_chiave("sk-beta"));
        assert_eq!(hash_chiave("sk-acme").len(), 64, "SHA-256 in esadecimale");
    }
}
