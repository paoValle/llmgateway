//! Il denaro: micro-dollari interi, prezzi per modello, costo di una richiesta.
//!
//! Le stesse regole di `agentloop`, per lo stesso motivo: `0.1 + 0.2 !== 0.3`, e su
//! un fatturato la differenza è un buco. Nessun `f64` attraversa questo modulo.
//!
//! Qui non c'è prenotazione: quella sta in [`crate::budget`], che è dove vivono i
//! tenant. Questo modulo sa solo trasformare token in denaro.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Un importo in micro-dollari. Intero, mai `f64`.
///
/// `1 USD = 1 000 000 µUSD`. La granularità di un micro-dollaro è di gran lunga più
/// fine di quella di qualsiasi listino, e un tetto espresso in interi non può
/// accumulare errore di arrotondamento nel tempo.
pub type MicroUsd = u64;

/// Un dollaro, in micro-dollari.
#[must_use]
pub fn usd(amount: f64) -> Option<MicroUsd> {
    if !amount.is_finite() || amount < 0.0 {
        return None;
    }
    // `round` invece di un troncamento: 0.9999999999999999 diventerebbe 0, e un
    // prezzo che diventa zero per arrotondamento è un prezzo che non c'è.
    #[allow(clippy::cast_sign_loss)] // il segno è già escluso dai controlli sopra
    Some((amount * 1_000_000.0).round() as MicroUsd)
}

/// Prezzi di un modello, in micro-dollari per **milione** di token.
///
/// Unità comoda perché è quella con cui i provider pubblicano i listini: non
/// importa se un token costa 0,15 `µUSD` o 15 `µUSD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    /// Per milione di token di input.
    pub input: MicroUsd,
    /// Per milione di token di output.
    pub output: MicroUsd,
}

impl Price {
    /// Un prezzo di zero. Serve solo per "prezzo sconosciuto, non conteggiare".
    pub const ZERO: Self = Self {
        input: 0,
        output: 0,
    };

    /// Quanto costa una richiesta che consuma questi token.
    ///
    /// Aritmetica intera, arrotondamento **per eccesso**. Un errore di un
    /// micro-dollaro a favore del sistema costa meno di una fattura che nessuno
    /// riesce a spiegare: la somma di tanti arrotondamenti per difetto, su un
    /// volume alto, è la differenza fra una previsione e una sorpresa.
    ///
    /// Il sovraccarico qui dentro è saturante, non va in panic: con prezzi e
    /// conteggi al massimo, `token × prezzo` in `u64` trabocca, e un gateway che
    /// entra in panic mentre calcola un conto non deve accadere.
    #[must_use]
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> MicroUsd {
        let input = u128::from(input_tokens).saturating_mul(u128::from(self.input));
        let output = u128::from(output_tokens).saturating_mul(u128::from(self.output));
        // somma in u128: con u64 il prodotto token × prezzo trabocca su un modello
        // costoso e una risposta lunga
        div_ceil(input.saturating_add(output), 1_000_000)
    }

    /// Il prezzo massimo fra due, componente per componente.
    ///
    /// Serve al fallback per i modelli sconosciuti: valutarli al prezzo più alto
    /// noto significa "ipotizziamo il peggio", che è l'unica ipotesi sensata per
    /// un tetto di spesa.
    #[must_use]
    pub fn max_componentwise(self, other: Self) -> Self {
        Self {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
        }
    }
}

/// Divisione intera per eccesso. `div_ceil` è stabile dalla 1.73, ma il progetto
/// dichiara `rust-version = 1.80`: la si scrive qui per non dipendere da un
/// dettaglio della versione in un posto dove l'aritmetica è il punto.
fn div_ceil(numerator: u128, denominator: u128) -> MicroUsd {
    numerator.div_ceil(denominator).min(u128::from(u64::MAX)) as MicroUsd
}

/// I prezzi per modello.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    prices: BTreeMap<String, Price>,
    /// Il prezzo più alto noto: il fallback per i modelli che non ci sono.
    worst: Option<Price>,
}

impl PriceTable {
    /// Costruisce da una mappa modello → prezzo.
    #[must_use]
    pub fn new(prices: BTreeMap<String, Price>) -> Self {
        let worst = prices.values().copied().reduce(Price::max_componentwise);
        Self { prices, worst }
    }

    /// Vuota: ogni modello sconosciuto costa zero, e ogni costo è zero.
    ///
    /// Esiste per i test e per un gateway senza listini. **In produzione una tabella
    /// vuota è un errore di configurazione**, e `Config::validate` lo segnala.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Il prezzo dichiarato di un modello, se c'è.
    #[must_use]
    pub fn get(&self, model: &str) -> Option<Price> {
        self.prices.get(model).copied()
    }

    /// Il prezzo da usare per un modello: dichiarato, o il peggiore noto.
    ///
    /// Il fallback pessimistico è deliberato: un modello nuovo che entra in
    /// produzione è il momento esatto in cui un prezzo a zero farebbe sembrare che
    /// il budget protegga mentre non protegge niente.
    #[must_use]
    pub fn resolve(&self, model: &str) -> Price {
        self.get(model).or(self.worst).unwrap_or(Price::ZERO)
    }

    /// `true` se il modello ha un prezzo dichiarato.
    ///
    /// Serve al logging per distinguere "prezzo reale" da "stima pessimista".
    #[must_use]
    pub fn knows(&self, model: &str) -> bool {
        self.prices.contains_key(model)
    }

    /// Quanti modelli hanno un prezzo dichiarato.
    #[must_use]
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// `true` se nessun modello ha un prezzo.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }
}

/// Token consumati da una richiesta, come li riporta il provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Token di input (prompt).
    #[serde(default)]
    pub input_tokens: u64,
    /// Token di output (completamento).
    #[serde(default)]
    pub output_tokens: u64,
}

impl Usage {
    /// Costo di questi token al prezzo dato.
    #[must_use]
    pub fn cost(&self, price: Price) -> MicroUsd {
        price.cost(self.input_tokens, self.output_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabella() -> PriceTable {
        let mut m = BTreeMap::new();
        m.insert(
            "economico".to_owned(),
            Price {
                input: 150,
                output: 600,
            },
        );
        m.insert(
            "costoso".to_owned(),
            Price {
                input: 2_500,
                output: 10_000,
            },
        );
        PriceTable::new(m)
    }

    #[test]
    fn un_dollaro_sono_un_milione_di_micro_dollari() {
        assert_eq!(usd(1.0), Some(1_000_000));
        assert_eq!(usd(0.25), Some(250_000));
        assert_eq!(usd(0.0), Some(0));
    }

    #[test]
    fn un_prezzo_negativo_o_non_finito_non_e_un_prezzo() {
        assert_eq!(usd(-1.0), None);
        assert_eq!(usd(f64::NAN), None);
        assert_eq!(usd(f64::INFINITY), None);
    }

    #[test]
    fn il_costo_si_calcola_sul_listino_per_milione() {
        let p = Price {
            input: 3,
            output: 15,
        };
        // 1M + 1M token = 3 + 15 µUSD
        assert_eq!(p.cost(1_000_000, 1_000_000), 18);
        assert_eq!(p.cost(0, 0), 0);
    }

    #[test]
    fn il_costo_arrotonda_per_eccesso_mai_per_difetto() {
        let p = Price {
            input: 3,
            output: 15,
        };
        // 1 token da 3 µUSD/M = 0,000003 µUSD: deve valere 1, non 0
        assert_eq!(p.cost(1, 0), 1);
        assert_eq!(p.cost(0, 1), 1);
        assert_eq!(p.cost(1, 1), 1);
    }

    /// Prezzo realistico: 3 `µUSD` per milione di token è un listino che non esiste.
    const PREZZO: Price = Price {
        input: 150,
        output: 600,
    };
    const RICHIESTE: u64 = 1_000;
    const TOKEN_PER_RICHIESTA: u64 = 1_000;

    #[test]
    fn la_somma_di_tante_richieste_mai_sotto_conta_il_costo_reale() {
        let p = PREZZO;
        let conteggiato: MicroUsd = (0..RICHIESTE)
            .map(|_| p.cost(TOKEN_PER_RICHIESTA, 500))
            .sum();
        // il vero costo, arrotondato una volta sola su tutto
        let vero = p.cost(TOKEN_PER_RICHIESTA * RICHIESTE, 500 * RICHIESTE);

        assert!(
            conteggiato >= vero,
            "mai sotto: {conteggiato} < {vero} — il tetto non protegge più"
        );
        // e di quanto sbaglia: al più un micro-dollaro a richiesta, per l'arrotondamento
        let eccesso = conteggiato - vero;
        assert!(
            eccesso <= RICHIESTE,
            "sovrastima di {eccesso} su {RICHIESTE} richieste: più di 1 µUSD a richiesta"
        );
    }

    #[test]
    fn un_prodotto_overflow_non_panorama() {
        // token × prezzo in u64 non ci sta, e nemmeno la somma dei due in u128:
        // il risultato deve essere un numero, non un panic
        let p = Price {
            input: u64::MAX,
            output: u64::MAX,
        };
        let costo = p.cost(u64::MAX, u64::MAX);
        assert_eq!(costo, u64::MAX);
    }

    #[test]
    fn il_max_componentwise_tiene_il_peggior_lato() {
        let a = Price {
            input: 100,
            output: 5_000,
        };
        let b = Price {
            input: 900,
            output: 1,
        };
        let m = a.max_componentwise(b);
        assert_eq!(
            m,
            Price {
                input: 900,
                output: 5_000
            }
        );
    }

    #[test]
    fn un_modello_conosciuto_ha_il_suo_prezzo() {
        assert_eq!(tabella().resolve("costoso").input, 2_500);
        assert!(tabella().knows("costoso"));
    }

    #[test]
    fn un_modello_sconosciuto_vale_il_prezzo_piu_alto_che_conosciamo() {
        let prezzo = tabella().resolve("modello-del-futuro");
        // non zero: prezzo zero significa "il budget protegge" e non è vero
        assert_eq!(prezzo.input, 2_500);
        assert_eq!(prezzo.output, 10_000);
        assert!(!tabella().knows("modello-del-futuro"));
    }

    #[test]
    fn una_tabella_vuota_da_zero_e_lo_dice() {
        assert!(PriceTable::empty().is_empty());
        assert_eq!(PriceTable::empty().resolve("qualsiasi"), Price::ZERO);
    }

    #[test]
    fn il_usage_si_deserializza_dal_formato_del_provider() {
        // il campo si chiama prompt_tokens upstream: la rinomina è compito dell'adapter
        let json = r#"{"input_tokens":120,"output_tokens":40}"#;
        let u: Usage = serde_json::from_str(json).expect("usage valido");
        assert_eq!(
            u,
            Usage {
                input_tokens: 120,
                output_tokens: 40
            }
        );
    }
}
