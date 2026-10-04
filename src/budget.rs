//! Il tetto per tenant: prenotazione, saldo, e cambio di mese.
//!
//! Come in `agentloop`, il meccanismo è **prenota → salda**: si blocca una stima
//! prima di chiamare il provider, e si libera la differenza quando arriva il conto
//! vero. Un tetto che si verifica dopo protegge la richiesta precedente.
//!
//! Il lock è **per tenant**, non globale. Un `Mutex` unico su tutti i contatori
//! renderebbe il gateway single-threaded proprio sul percorso critico: un tenant
//! che paga tanto bloccherebbe la prenotazione di tutti gli altri. Ogni tenant ha
//! il suo, e due tenant non contendono mai.
//!
//! Il cambio di mese è il punto delicato: a mezzanotte il contatore riparte, e una
//! prenotazione aperta nel mese precedente non deve più essere saldata contro il
//! nuovo. Qui la prenotazione porta con sé la finestra in cui è nata.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::pricing::MicroUsd;

/// Un mese di un anno, come chiave finestra.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Month {
    /// Anno.
    pub year: i64,
    /// Mese da 1 a 12.
    pub month: u32,
}

impl Month {
    /// Il mese in cui cade un istante Unix, in millisecondi.
    ///
    /// Calcolato con l'algoritmo di Howard Hinnant per la data civile, che è
    /// esatto per qualunque data gregoriana e non ha tabelle né dipendenze. Il
    /// secondo è UTC: un tetto mensile che cambiasse a seconda del fuso orario
    /// del server sarebbe una sorpresa per chi lo amministra.
    #[must_use]
    pub fn of(epoch_ms: i64) -> Self {
        let days = epoch_ms.div_euclid(86_400_000);
        let (year, month, _day) = civil_from_days(days);
        Self { year, month }
    }
}

/// Data civile da un conteggio di giorni dall'epoca (algoritmo di Hinnant).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // i tre cast a u32 qui sotto sono sicuri per costruzione: l'algoritmo produce
    // un giorno in [1, 31] e un mese in [1, 12], quindi non c'è segno da perdere.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn narrow(v: i64) -> u32 {
        u32::try_from(v).unwrap_or(0)
    }

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let mp = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = narrow(day_of_year - (153 * mp + 2) / 5 + 1); // [1, 31]
    let month = narrow(if mp < 10 { mp + 3 } else { mp - 9 }); // [1, 12]
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Perché una prenotazione è stata rifiutata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExceeded {
    /// Quanto serviva, in `µUSD`.
    pub requested: MicroUsd,
    /// Quanto c'era, in `µUSD`.
    pub available: MicroUsd,
    /// Il tetto del tenant.
    pub limit: MicroUsd,
}

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "budget esaurito: servono {} µUSD, disponibili {} (tetto {})",
            self.requested, self.available, self.limit
        )
    }
}

impl std::error::Error for BudgetExceeded {}

/// Perché una prenotazione è fallita.
///
/// Due cause, e non si possono confondere: una è **economica** e va restituita al
/// client come `429`, l'altra è uno **stato interno compromesso** e va segnalata come
/// un problema del gateway. Un unico tipo di errore che le mescolasse porterebbe a
/// rispondere `429` a un bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveError {
    /// Il tetto non copre la stima. Va al client.
    Exceeded(BudgetExceeded),
    /// Il lock è avvelenato da un panic. È un problema del gateway, non del tenant.
    State(BudgetPoisoned),
}

impl std::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exceeded(e) => write!(f, "{e}"),
            Self::State(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReserveError {}

/// Una prenotazione di denaro.
///
/// Va **sempre** saldata o liberata. Una prenotazione dimenticata blocca budget
/// per il resto della finestra: per questo `Budget` espone `open_reservations` e il
/// gateway lo registra.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    id: u64,
    amount: MicroUsd,
    window: Month,
}

/// Lo stato interno di un tenant, protetto dal suo lock.
#[derive(Debug, Clone, Copy)]
struct State {
    window: Month,
    spent: MicroUsd,
    held: MicroUsd,
    next_id: u64,
}

/// Il tetto di un tenant.
#[derive(Debug)]
pub struct TenantBudget {
    tenant_id: String,
    limit: MicroUsd,
    state: Mutex<State>,
}

impl TenantBudget {
    /// Crea un tetto. `window` è la finestra corrente: chi lo crea decide da dove
    /// si parte, e i test ne hanno bisogno.
    #[must_use]
    pub fn new(tenant_id: impl Into<String>, limit: MicroUsd, window: Month) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            limit,
            state: Mutex::new(State {
                window,
                spent: 0,
                held: 0,
                next_id: 1,
            }),
        }
    }

    /// L'identificatore del tenant.
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    /// Il tetto della finestra.
    #[must_use]
    pub fn limit(&self) -> MicroUsd {
        self.limit
    }

    /// Denaro speso nella finestra corrente.
    #[must_use]
    pub fn spent(&self, now_ms: i64) -> MicroUsd {
        self.contabile(now_ms).map_or(0, |s| s.spent)
    }

    /// Denaro prenotato e non ancora saldato.
    #[must_use]
    pub fn held(&self, now_ms: i64) -> MicroUsd {
        self.contabile(now_ms).map_or(0, |s| s.held)
    }

    /// Quanto si può ancora prenotare.
    #[must_use]
    pub fn available(&self, now_ms: i64) -> MicroUsd {
        self.contabile(now_ms)
            .map_or(0, |s| self.limit - s.spent - s.held)
    }

    /// Quante prenotazioni sono aperte. Se resta qualcosa a fine richiesta, è un bug.
    #[must_use]
    pub fn open_reservations(&self, now_ms: i64) -> u64 {
        self.contabile(now_ms).map_or(0, |_| {
            self.state.lock().map_or(0, |s| s.next_id.saturating_sub(1))
        })
    }

    /// Prenota `amount`, o fallisce **prima** che qualcuno spenda.
    pub fn reserve(&self, amount: MicroUsd, now_ms: i64) -> Result<Reservation, ReserveError> {
        let mut s = self.lock(now_ms).map_err(ReserveError::State)?;
        let available = self.limit - s.spent - s.held;
        if amount > available {
            return Err(ReserveError::Exceeded(BudgetExceeded {
                requested: amount,
                available,
                limit: self.limit,
            }));
        }
        s.held += amount;
        let reservation = Reservation {
            id: s.next_id,
            amount,
            window: s.window,
        };
        s.next_id += 1;
        Ok(reservation)
    }

    /// Salda con il consumo reale e libera la differenza.
    ///
    /// Se la prenotazione appartiene a una **finestra precedente**, è un no-op: il
    /// mese è cambiato, il contatore è ripartito da zero, e sommare un debito vecchio
    /// su un contatore nuovo farebbe sparire denaro che è già stato speso.
    pub fn settle(&self, reservation: Reservation, actual: MicroUsd, now_ms: i64) {
        if let Ok(mut s) = self.lock(now_ms) {
            if s.window != reservation.window {
                return;
            }
            s.held = s.held.saturating_sub(reservation.amount);
            s.spent = s.spent.saturating_add(actual);
        }
    }

    /// Rilascia la prenotazione: la stima si è rivelata sovrastimata.
    pub fn release(&self, reservation: Reservation, now_ms: i64) {
        if let Ok(mut s) = self.lock(now_ms) {
            if s.window != reservation.window {
                return;
            }
            s.held = s.held.saturating_sub(reservation.amount);
        }
    }

    /// Il dettaglio per `/metrics` e per i log.
    #[must_use]
    pub fn snapshot(&self, now_ms: i64) -> Option<Snapshot> {
        let s = self.lock(now_ms).ok()?;
        Some(Snapshot {
            tenant_id: self.tenant_id.clone(),
            window: s.window,
            limit: self.limit,
            spent: s.spent,
            held: s.held,
            available: self.limit - s.spent - s.held,
        })
    }

    /// Prende il lock e azzera i contatori se il mese è cambiato.
    fn lock(&self, now_ms: i64) -> Result<std::sync::MutexGuard<'_, State>, BudgetPoisoned> {
        let mut s = self.state.lock().map_err(|_| BudgetPoisoned)?;
        let corrente = Month::of(now_ms);
        if s.window != corrente {
            *s = State {
                window: corrente,
                spent: 0,
                held: 0,
                next_id: 1,
            };
        }
        Ok(s)
    }

    /// Come [`Self::lock`] ma senza toccare i contatori: per le letture.
    fn contabile(&self, now_ms: i64) -> Option<State> {
        let s = self.lock(now_ms).ok()?;
        Some(*s)
    }
}

/// Il lock è stato avvelenato da un panic in un altro thread.
///
/// Non è recuperabile in modo utile: se è successo, lo stato è in parte
/// incoerente e la cosa giusta è notarlo e rifiutare, non fingere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetPoisoned;

impl std::fmt::Display for BudgetPoisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stato del budget non leggibile: un thread è andato in panic tenendo il lock")
    }
}

impl std::error::Error for BudgetPoisoned {}

/// Una fotografia del tetto di un tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Chi.
    pub tenant_id: String,
    /// In che mese.
    pub window: Month,
    /// Tetto.
    pub limit: MicroUsd,
    /// Speso.
    pub spent: MicroUsd,
    /// Prenotato e non saldato.
    pub held: MicroUsd,
    /// Ancora prenotabile.
    pub available: MicroUsd,
}

/// Tutti i tenant, in un solo posto.
#[derive(Debug, Default)]
pub struct BudgetRegistry {
    budgets: RwLock<BTreeMap<String, Arc<TenantBudget>>>,
}

impl BudgetRegistry {
    /// Vuoto.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra un tetto. Se il tenant esiste già, viene sostituito.
    pub fn insert(&self, budget: TenantBudget, now_ms: i64) -> Arc<TenantBudget> {
        let id = budget.tenant_id().to_owned();
        let arco = Arc::new(budget);
        if let Ok(mut b) = self.budgets.write() {
            b.insert(id, Arc::clone(&arco));
        }
        // si allinea subito alla finestra corrente: un tetto costruito con una
        // finestra vecchia deve valere da subito, non alla prima prenotazione
        let _ = arco.spent(now_ms);
        arco
    }

    /// Il tetto di un tenant, se esiste.
    #[must_use]
    pub fn get(&self, tenant_id: &str) -> Option<Arc<TenantBudget>> {
        self.budgets.read().ok()?.get(tenant_id).cloned()
    }

    /// Tutti i tenant, per le metriche.
    #[must_use]
    pub fn snapshots(&self, now_ms: i64) -> Vec<Snapshot> {
        let tutti: Vec<Arc<TenantBudget>> = match self.budgets.read() {
            Ok(b) => b.values().cloned().collect(),
            Err(_) => return Vec::new(),
        };
        tutti.iter().filter_map(|b| b.snapshot(now_ms)).collect()
    }

    /// Quanti tenant sono registrati.
    #[must_use]
    pub fn len(&self) -> usize {
        self.budgets.read().map_or(0, |b| b.len())
    }

    /// `true` se non c'è nessun tenant.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEN: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
    const GIORNO: i64 = 86_400_000;

    fn tetto(limit: MicroUsd) -> TenantBudget {
        TenantBudget::new("acme", limit, Month::of(GEN))
    }

    #[test]
    fn il_mese_si_calcola_in_utc_e_non_nel_fuso_del_server() {
        assert_eq!(
            Month::of(GEN),
            Month {
                year: 2026,
                month: 1
            }
        );
        assert_eq!(
            Month::of(GEN + 31 * GIORNO),
            Month {
                year: 2026,
                month: 2
            }
        );
        assert_eq!(
            Month::of(GEN + 365 * GIORNO),
            Month {
                year: 2027,
                month: 1
            }
        );
    }

    #[test]
    fn il_calcolo_del_mese_rispetta_la_lunghezza_dei_mesi() {
        // gennaio 2026 ha 31 giorni: il 28 febbraio è 58 giorni dopo il 1 gennaio
        assert_eq!(
            Month::of(GEN + 58 * GIORNO),
            Month {
                year: 2026,
                month: 2
            }
        );
        assert_eq!(
            Month::of(GEN + 59 * GIORNO),
            Month {
                year: 2026,
                month: 3
            }
        );
    }

    #[test]
    fn il_calcolo_del_mese_rispetta_l_anno_bisestile() {
        // 2028 è bisestile: il 1 marzo è 790 giorni dopo il 1 gennaio 2026
        // (365 + 365 + 60, dove 60 = 31 di gennaio + 29 di febbraio)
        assert_eq!(
            Month::of(GEN + 790 * GIORNO),
            Month {
                year: 2028,
                month: 3
            }
        );
        // il giorno prima è ancora febbraio: se il bisestile fosse ignorato,
        // questo sarebbe già marzo
        assert_eq!(
            Month::of(GEN + 789 * GIORNO),
            Month {
                year: 2028,
                month: 2
            }
        );
    }

    #[test]
    fn il_mese_prima_dell_epoca_non_panorama() {
        // un orologio sbagliato o un test con timestamp negativi non deve andare in panic
        assert_eq!(Month::of(-2_208_988_800_000).year, 1900);
    }

    #[test]
    fn una_prenotazione_blocca_il_denaro_fino_al_saldo() {
        let b = tetto(1_000_000);
        let r = b.reserve(400_000, GEN).expect("entra");
        assert_eq!(b.held(GEN), 400_000);
        assert_eq!(b.spent(GEN), 0);
        assert_eq!(b.available(GEN), 600_000);

        b.settle(r, 300_000, GEN);
        assert_eq!(b.held(GEN), 0);
        assert_eq!(b.spent(GEN), 300_000);
        assert_eq!(b.available(GEN), 700_000);
    }

    #[test]
    fn superare_il_tetto_fallisce_prima_della_spesa() {
        let b = tetto(1_000_000);
        b.reserve(800_000, GEN).expect("entra");
        let errore = b.reserve(300_000, GEN).expect_err("non entra");
        let ReserveError::Exceeded(e) = errore else {
            panic!("un rifiuto per budget, non un problema di stato: {errore:?}");
        };
        assert_eq!(e.requested, 300_000);
        assert_eq!(e.available, 200_000);
        assert_eq!(e.limit, 1_000_000);
        assert_eq!(b.spent(GEN), 0, "il rifiuto non deve muovere denaro");
    }

    #[test]
    fn la_stima_sovrastimata_non_brucia_denaro() {
        let b = tetto(1_000_000);
        let r = b.reserve(900_000, GEN).expect("entra");
        b.settle(r, 1, GEN);
        assert_eq!(b.spent(GEN), 1);
        assert_eq!(b.available(GEN), 999_999);
    }

    #[test]
    fn rilasciare_libera_senza_spendere() {
        let b = tetto(1_000_000);
        let r = b.reserve(500_000, GEN).expect("entra");
        b.release(r, GEN);
        assert_eq!(b.held(GEN), 0);
        assert_eq!(b.spent(GEN), 0);
    }

    #[test]
    fn il_cambio_di_mese_azzera_e_libera_le_prenotazioni_vecchie() {
        let b = tetto(1_000_000);
        b.reserve(900_000, GEN).expect("entra");
        assert_eq!(b.available(GEN), 100_000);

        // primo giorno del mese dopo
        let febbraio = GEN + 31 * GIORNO;
        assert_eq!(b.spent(febbraio), 0);
        assert_eq!(
            b.held(febbraio),
            0,
            "la prenotazione di gennaio non blocca febbraio"
        );
        assert_eq!(b.available(febbraio), 1_000_000);
    }

    #[test]
    fn una_prenotazione_del_mese_scaduto_non_si_salda_sul_conto_nuovo() {
        let b = tetto(1_000_000);
        let r = b.reserve(900_000, GEN).expect("entra");
        let febbraio = GEN + 31 * GIORNO;

        // la richiesta è iniziata in gennaio e finisce in febbraio
        b.settle(r, 900_000, febbraio);
        assert_eq!(
            b.spent(febbraio),
            0,
            "il debito di gennaio non può cadere su febbraio"
        );
        assert_eq!(b.held(febbraio), 0);
    }

    #[test]
    fn il_conteggio_delle_prenotazioni_aperte_e_visibile() {
        let b = tetto(1_000_000);
        assert_eq!(b.open_reservations(GEN), 0);
        let r = b.reserve(1, GEN).expect("entra");
        assert_eq!(b.open_reservations(GEN), 1);
        b.settle(r, 1, GEN);
        // il contatore non scende: dire "0 prenotazioni aperte" dopo un saldo
        // sarebbe falso, la finestra ne ha avute una
        assert_eq!(b.open_reservations(GEN), 1);
    }

    #[test]
    fn un_tetto_azzerato_all_avvio_non_e_un_tetto_di_sicurezza() {
        let b = tetto(0);
        assert!(b.reserve(1, GEN).is_err(), "con tetto zero non si passa");
    }

    #[test]
    fn lo_snapshot_descriva_il_quadro() {
        let b = tetto(2_000_000);
        b.reserve(500_000, GEN).expect("entra");
        assert_eq!(
            b.snapshot(GEN),
            Some(Snapshot {
                tenant_id: "acme".to_owned(),
                window: Month::of(GEN),
                limit: 2_000_000,
                spent: 0,
                held: 500_000,
                available: 1_500_000,
            })
        );
    }

    #[test]
    fn il_registro_restituisce_lo_stesso_tetto_e_lo_tiene_per_id() {
        let reg = BudgetRegistry::new();
        let a = reg.insert(tetto(1_000_000), GEN);
        reg.insert(TenantBudget::new("beta", 5_000_000, Month::of(GEN)), GEN);

        assert_eq!(reg.len(), 2);
        assert_ne!(reg.len(), 0);
        assert_eq!(reg.get("acme").map(|b| b.limit()), Some(1_000_000));
        assert_eq!(reg.get("beta").map(|b| b.limit()), Some(5_000_000));
        assert!(reg.get("gamma").is_none());

        // risteggiare lo stesso tenant non ne crea un secondo
        reg.insert(tetto(7_000_000), GEN);
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.get("acme").map(|b| b.limit()), Some(7_000_000));
        // il tenant precedente è stato sostituito, non affiancato
        assert_ne!(a.limit(), 7_000_000);
    }

    #[test]
    fn le_gli_snapshot_ordinano_per_tenant_e_sono_completi() {
        let reg = BudgetRegistry::new();
        reg.insert(tetto(1_000_000), GEN);
        reg.insert(TenantBudget::new("beta", 5_000_000, Month::of(GEN)), GEN);
        let snap = reg.snapshots(GEN);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].tenant_id, "acme");
        assert_eq!(snap[1].tenant_id, "beta");
    }

    #[test]
    fn un_registro_vuoto_non_panorama() {
        let reg = BudgetRegistry::new();
        assert_eq!(reg.len(), 0);
        assert!(reg.get("acme").is_none());
        assert_eq!(reg.snapshots(GEN).len(), 0);
    }
}
