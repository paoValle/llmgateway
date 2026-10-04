//! Il metering: quanto è costato, e a chi.
//!
//! Un modulo con una proprietà sola, che è il motivo per cui esiste come modulo
//! separato: **non ha modo di fallire.** Nessun `Result`, nessun panico, nessuna
//! dipendenza che possa mancare. Se qualcosa non torna — un modello non in listino,
//! un contatore che trabocca, un tenant mai visto — la funzione degrada, e il fatto
//! che ha degradato è visibile in [`Meter::errors`].
//!
//! Il perché è in ADR 0004 e vale la pena ripeterlo: se il contabile fa cadere il
//! servizio, l'utente non riceve la risposta e il ticket che arriva è "il gateway è
//! lento". Il contatore perso si recupera dalla fattura del provider. La risposta
//! persa no.
//!
//! Due precisioni, e la differenza è voluta (ADR 0005):
//!
//! - **il denaro è esatto**, per tenant. Non è campionato: è il numero per cui il
//!   gateway esiste.
//! - **le metriche operative sono campionate** 1 su N. Contarle tutte costa un
//!   lock su ogni richiesta, e il contatore globale diventa un collo di bottiglia.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::pricing::{MicroUsd, Price, PriceTable, Usage};
use crate::request::RequestShape;

/// Un tenant, con i suoi contatori esatti.
#[derive(Debug, Default)]
struct TenantTotals {
    /// Denaro speso. **Mai campionato**: è il dato per cui il gateway esiste.
    spent: AtomicU64,
    /// Richieste servite con successo.
    served: AtomicU64,
    /// Richieste respinte dal tetto.
    budget_denied: AtomicU64,
    /// Richieste che non sono arrivate da nessun provider.
    failed: AtomicU64,
}

/// Come è finita una richiesta, dal punto di vista del conto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Esito {
    /// Risposta 2xx da un provider.
    Servito {
        /// Chi ha risposto.
        provider: String,
        /// Il modello di listino usato. Se il modello non era in tabella è il prezzo
        /// peggiore noto, non zero.
        prezzo_stimato: bool,
    },
    /// Rifiutata dal tetto, senza che nessun provider fosse chiamato.
    TettoNegato,
    /// Non è arrivata risposta da nessun provider.
    Fallito {
        /// L'ultimo provider provato, per il log.
        provider: String,
    },
}

/// Il contatore.
#[derive(Debug)]
pub struct Meter {
    prices: PriceTable,
    /// I tenant incontrati. Crescono con l'uso, non con la configurazione: è la
    /// differenza fra un gateway che conosce i suoi clienti e uno che li scopre.
    totals: RwLock<BTreeMap<String, Arc<TenantTotals>>>,
    /// Tasso di campionamento delle metriche operative. 1 = esatte.
    sample_rate: u64,
    /// Contatore di campionamento: ogni N eventi, uno viene contato.
    ticker: AtomicU64,
    /// Volte in cui il metering ha dovuto degradare. Sale invece di far fallire.
    errors: AtomicU64,
    /// Richieste viste in assoluto, anche quando non campionate.
    seen: AtomicU64,
    /// Conti calcolati su un modello fuori listino: vanno verificati a mano.
    estimated: AtomicU64,
}

impl Meter {
    /// Crea un contatore. `sample_rate` è 1 su N per le metriche operative.
    #[must_use]
    pub fn new(prices: PriceTable, sample_rate: u64) -> Self {
        Self {
            prices,
            totals: RwLock::new(BTreeMap::new()),
            sample_rate: sample_rate.max(1),
            ticker: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            estimated: AtomicU64::new(0),
        }
    }

    /// Il prezzo che verrà usato per un modello, e se è quello dichiarato.
    ///
    /// `prezzo_stimato = true` significa che il modello non era in listino e si sta
    /// usando il peggiore noto: è un segnale di allarme, non un dettaglio.
    #[must_use]
    pub fn prezzo_per(&self, model: &str) -> (Price, bool) {
        (self.prices.resolve(model), !self.prices.knows(model))
    }

    /// Registra una richiesta servita. **Non può fallire.**
    pub fn registr(&self, tenant: &str, forma: &RequestShape, usage: Usage, esito: &Esito) {
        self.seen.fetch_add(1, Ordering::Relaxed);

        // l'unico posto in cui il metering può degradare, e dichiara di averlo fatto
        let (prezzo, stimato) = if let Some(modello) = forma.model.as_deref() {
            self.prezzo_per(modello)
        } else {
            // nessun modello: nessun listino, nessun costo. Si conta l'errore e si
            // va avanti — sarà il provider a rispondere che non capisce la richiesta
            self.errors.fetch_add(1, Ordering::Relaxed);
            (Price::ZERO, true)
        };
        if stimato {
            // un modello fuori listino è un allarme, non un dettaglio: il tetto è
            // calcolato sul prezzo peggiore e il conto va verificato
            self.estimated.fetch_add(1, Ordering::Relaxed);
        }

        let costo = prezzo.cost(usage.input_tokens, usage.output_tokens);

        // il denaro è esatto, e non è campionato: è il numero per cui il gateway esiste
        self.totali_del(tenant)
            .spent
            .fetch_add(costo, Ordering::Relaxed);

        if !self.campiona() {
            return;
        }
        let contatori = self.totali_del(tenant);
        match esito {
            Esito::Servito { .. } => {
                contatori.served.fetch_add(1, Ordering::Relaxed);
            }
            Esito::TettoNegato => {
                contatori.budget_denied.fetch_add(1, Ordering::Relaxed);
            }
            &Esito::Fallito { .. } => {
                contatori.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Registra un rifiuto per tetto. Il provider non è stato chiamato: il denaro
    /// speso è zero, ma la richiesta è comunque un'informazione che serve.
    pub fn registr_tetto_negato(&self, tenant: &str) {
        self.seen.fetch_add(1, Ordering::Relaxed);
        let contatori = self.totali_del(tenant);
        if self.campiona() {
            contatori.budget_denied.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Quanto ha speso un tenant. **Esatto**, mai campionato.
    ///
    /// Un tenant mai visto ha speso zero, non "non lo so": il gateway lo conosce
    /// dalla configurazione, e se non c'è nessun errore registrato non ha speso.
    #[must_use]
    pub fn speso(&self, tenant: &str) -> MicroUsd {
        self.totali_del(tenant).spent.load(Ordering::Relaxed)
    }

    /// Le fotografie di tutti i tenant incontrati, per `/metrics`.
    #[must_use]
    pub fn snapshots(&self) -> Vec<TenantSnapshot> {
        let tutti: Vec<Arc<TenantTotals>> = match self.totals.read() {
            Ok(t) => t.values().cloned().collect(),
            Err(_) => return Vec::new(),
        };
        tutti
            .iter()
            .map(|t| TenantSnapshot {
                spent: t.spent.load(Ordering::Relaxed),
                served: t.served.load(Ordering::Relaxed),
                budget_denied: t.budget_denied.load(Ordering::Relaxed),
                failed: t.failed.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Quante volte il metering ha dovuto degradare. Su zero, tutto è andato.
    #[must_use]
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Conti calcolati su un modello **fuori listino**, con il prezzo peggiore noto.
    ///
    /// Su zero, ogni modello che è passato è un modello che si conosce. Su un numero
    /// alto, o il listino va aggiornato, o il gateway sta servendo qualcosa che non
    /// era previsto.
    #[must_use]
    pub fn estimated(&self) -> u64 {
        self.estimated.load(Ordering::Relaxed)
    }

    /// Quante richieste sono passate, campionate o no.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen.load(Ordering::Relaxed)
    }

    /// Il tasso di campionamento effettivo: `1` significa che ogni metrica è esatta.
    #[must_use]
    pub fn sample_rate(&self) -> u64 {
        self.sample_rate
    }

    /// I totali di un tenant, creandoli se non esistono.
    ///
    /// Se il lock è avvelenato si registra un errore e si restituisce un contatore
    /// "usa e getta": **la richiesta viene comunque servita**, che è il punto.
    fn totali_del(&self, tenant: &str) -> Arc<TenantTotals> {
        if let Ok(t) = self.totals.read() {
            if let Some(esistenti) = t.get(tenant) {
                return Arc::clone(esistenti);
            }
        }

        let nuovo = Arc::new(TenantTotals::default());
        if let Ok(mut t) = self.totals.write() {
            return Arc::clone(
                t.entry(tenant.to_owned())
                    .or_insert_with(|| Arc::clone(&nuovo)),
            );
        }
        // il lock è avvelenato da un panic altrui: si degrada, si conta l'errore, e la
        // richiesta va avanti con un contatore usa-e-getta
        self.errors.fetch_add(1, Ordering::Relaxed);
        nuovo
    }

    /// Tocca il contatore di campionamento e dice se questo evento va contato.
    fn campiona(&self) -> bool {
        let n = self.ticker.fetch_add(1, Ordering::Relaxed) + 1;
        n % self.sample_rate == 0
    }
}

/// La fotografia di un tenant per `/metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantSnapshot {
    /// Denaro speso, **esatto**.
    pub spent: MicroUsd,
    /// Richieste servite, campionate.
    pub served: u64,
    /// Rifiuti per tetto, campionati.
    pub budget_denied: u64,
    /// Fallite, campionate.
    pub failed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::micros;

    fn prezzi() -> PriceTable {
        let mut m = BTreeMap::new();
        m.insert(
            "economico".to_owned(),
            Price {
                input: micros(150),
                output: micros(600),
            },
        );
        m.insert(
            "costoso".to_owned(),
            Price {
                input: micros(2_500),
                output: micros(10_000),
            },
        );
        PriceTable::new(m)
    }

    fn forma(modello: &str) -> RequestShape {
        RequestShape {
            model: Some(modello.to_owned()),
            max_output_tokens: Some(1_000),
            stream: false,
            estimated_input_tokens: 1_000,
            body_bytes: 4_000,
        }
    }

    fn meter() -> Meter {
        Meter::new(prezzi(), 1)
    }

    #[test]
    fn il_conto_di_un_tenant_è_esatto() {
        let m = meter();
        m.registr(
            "acme",
            &forma("economico"),
            Usage {
                input_tokens: 1_000,
                output_tokens: 500,
            },
            &Esito::Servito {
                provider: "a".to_owned(),
                prezzo_stimato: false,
            },
        );

        // 1000×150/1e6 + 500×600/1e6 = 0.15 + 0.3 = 0.45 µUSD → 1 per eccesso
        assert_eq!(m.speso("acme"), 1);
    }

    #[test]
    fn i_conti_di_due_tenant_non_si_mescolano() {
        let m = meter();
        m.registr(
            "acme",
            &forma("costoso"),
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 0,
            },
            &Esito::Servito {
                provider: "a".to_owned(),
                prezzo_stimato: false,
            },
        );
        m.registr(
            "beta",
            &forma("economico"),
            Usage {
                input_tokens: 1,
                output_tokens: 0,
            },
            &Esito::Servito {
                provider: "a".to_owned(),
                prezzo_stimato: false,
            },
        );

        assert_eq!(m.speso("acme"), 2_500);
        assert_eq!(m.speso("beta"), 1);
        assert_eq!(m.speso("gamma"), 0, "un tenant mai visto ha speso zero");
    }

    #[test]
    fn un_modello_sconosciuto_si_conta_al_prezzo_peggiore_e_lo_dice() {
        let m = meter();
        let (prezzo, stimato) = m.prezzo_per("modello-del-futuro");
        assert!(stimato, "il gateway deve sapere che sta stimando");
        assert_eq!(
            prezzo,
            Price {
                input: micros(2_500),
                output: micros(10_000)
            }
        );
        assert!(!m.prezzo_per("economico").1);
    }

    #[test]
    fn una_forma_senza_modello_non_panorama_e_contabilizza_zero() {
        let m = meter();
        let mut f = forma("economico");
        f.model = None;
        m.registr(
            "acme",
            &f,
            Usage {
                input_tokens: 1_000,
                output_tokens: 500,
            },
            &Esito::Servito {
                provider: "a".to_owned(),
                prezzo_stimato: false,
            },
        );
        // senza modello non c'è prezzo: si conta zero e si registra l'errore,
        // ma la richiesta è comunque stata servita
        assert_eq!(m.speso("acme"), 0);
        assert_eq!(m.errors(), 1);
    }

    #[test]
    fn il_campionamento_risparmia_le_metriche_ma_non_il_denaro() {
        // è la distinzione di ADR 0005: contare ogni richiesta su un contatore globale
        // costa un lock su ogni richiesta, e il denaro deve restare esatto
        let m = Meter::new(prezzi(), 100);
        for _ in 0..1_000 {
            m.registr(
                "acme",
                &forma("costoso"),
                Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 0,
                },
                &Esito::Servito {
                    provider: "a".to_owned(),
                    prezzo_stimato: false,
                },
            );
        }

        let snap = m.snapshots();
        assert_eq!(snap.len(), 1);
        // il denaro è esatto: 1000 richieste da 2 500 µUSD ciascuna
        assert_eq!(snap[0].spent, 2_500 * 1_000);
        // le metriche operative sono stimate: 1000 eventi con tasso 100 → 10
        assert_eq!(snap[0].served, 10);
        assert_eq!(m.seen(), 1_000, "le richieste viste si contano tutte");
    }

    #[test]
    fn un_tasso_di_campionamento_di_uno_significa_esatto() {
        let m = Meter::new(prezzi(), 1);
        for _ in 0..50 {
            m.registr(
                "acme",
                &forma("economico"),
                Usage::default(),
                &Esito::Servito {
                    provider: "a".to_owned(),
                    prezzo_stimato: false,
                },
            );
        }
        assert_eq!(m.snapshots()[0].served, 50);
        assert_eq!(m.sample_rate(), 1);
    }

    #[test]
    fn un_tasso_di_zero_significa_contare_tutto_non_niente() {
        assert_eq!(Meter::new(prezzi(), 0).sample_rate(), 1);
    }

    #[test]
    fn i_rifiuti_per_tetto_si_contono_ma_non_spendono() {
        let m = meter();
        m.registr_tetto_negato("acme");
        m.registr_tetto_negato("acme");
        let snap = m.snapshots();
        assert_eq!(snap[0].budget_denied, 2);
        assert_eq!(
            snap[0].spent, 0,
            "un rifiuto non costa nulla: nessun provider è stato chiamato"
        );
    }

    #[test]
    fn i_fallimenti_si_contono_e_spentono_zero() {
        let m = meter();
        m.registr(
            "acme",
            &forma("costoso"),
            Usage::default(),
            &Esito::Fallito {
                provider: "a".to_owned(),
            },
        );
        let snap = m.snapshots();
        assert_eq!(snap[0].failed, 1);
        assert_eq!(snap[0].spent, 0);
    }

    #[test]
    fn i_conteggi_appaiono_nelle_snapshot_dopo_il_primo_uso() {
        let m = meter();
        assert_eq!(m.snapshots().len(), 0);
        m.registr_tetto_negato("nuovo");
        assert_eq!(m.snapshots().len(), 1);
    }
}
