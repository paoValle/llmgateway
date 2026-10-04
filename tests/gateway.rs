//! Il percorso completo, senza rete e senza server.
//!
//! Qui si verifica che i quattro passi stiano nell'ordine giusto e che ognuno
//! cambi il comportamento di chi viene dopo. Il test che conta più di tutti è
//! [`un_tetto_che_non_e_leggibile_non_ferma_il_cliente`]: se il contabile è rotto,
//! l'utente riceve comunque la sua risposta.

mod support;

use std::sync::Arc;
use std::time::Duration;

use llmgateway::auth::Autenticatore;
use llmgateway::budget::{BudgetRegistry, Month, TenantBudget};
use llmgateway::gateway::{Gateway, GatewayConfig, Richiesta};
use llmgateway::meter::Meter;
use llmgateway::pricing::{micros, usd, Price, PriceTable};
use llmgateway::router::Router;

use support::{condiviso, Comportamento, Finto};

const GEN: i64 = 1_767_225_600_000; // 2026-01-01
const TIMEOUT: Duration = Duration::from_secs(5);

fn prezzi() -> PriceTable {
    let mut m = std::collections::BTreeMap::new();
    m.insert(
        "gpt-4o-mini".to_owned(),
        Price {
            input: micros(150),
            output: micros(600),
        },
    );
    PriceTable::new(m)
}

/// Un gateway con un provider finto che risponde con un usage noto.
fn gateway(
    provider: Arc<dyn llmgateway::upstream::Upstream>,
    budget_usd: f64,
    modelli_autorizzati: &[&str],
) -> (Gateway, Arc<Meter>, Arc<TenantBudget>) {
    let prezzi = prezzi();
    let meter = Arc::new(Meter::new(prezzi.clone(), 1));
    let budget = BudgetRegistry::new();
    let tetto = budget.insert(
        TenantBudget::new(
            "acme",
            usd(budget_usd).expect("tetto valido"),
            Month::of(GEN),
        ),
        GEN,
    );
    let modelli: std::collections::BTreeSet<String> = modelli_autorizzati
        .iter()
        .map(|m| (*m).to_owned())
        .collect();
    let orologio = Arc::new(|| GEN);

    let gw = Gateway::new(GatewayConfig {
        autenticatore: Autenticatore::new(
            &[("acme".to_owned(), "sk-acme".to_owned())],
            &[("acme".to_owned(), modelli)],
        ),
        budget,
        meter: Arc::clone(&meter),
        router: Arc::new(Router::new(vec![provider], 4, TIMEOUT)),
        prezzi,
        max_output_default: 4_096,
        timeout: TIMEOUT,
        adesso: orologio,
    });
    (gw, meter, tetto)
}

/// La risposta 200 che il provider finto dà per default, con usage.
fn risposta_con_usage() -> Comportamento {
    Comportamento::Risponde(
        200,
        r#"{"choices":[{"message":{"content":"ciao"}}],"usage":{"prompt_tokens":1000,"completion_tokens":500}}"#
            .to_owned(),
    )
}

fn richiesta() -> Richiesta {
    Richiesta {
        chiave: Some("sk-acme".to_owned()),
        body: br#"{"model":"gpt-4o-mini","messages":[]}"#.to_vec(),
    }
}

// --- il percorso felice -----------------------------------------------------------

#[tokio::test]
async fn una_richiesta_valida_arriva_al_provider_e_ritorna() {
    let (gw, _, _) = gateway(
        condiviso(Finto::nuovo("a").con_comportamenti(vec![risposta_con_usage()])),
        10.0,
        &["gpt-4o-mini"],
    );
    let r = gw.gestisci(richiesta()).await;

    assert_eq!(r.status(), 200);
    assert_eq!(r.provider(), Some("a"));
    match r {
        llmgateway::gateway::Risposta::Intera { body, .. } => {
            assert!(String::from_utf8(body).unwrap().contains("ciao"));
        }
        llmgateway::gateway::Risposta::Flusso { .. } => {
            panic!("senza stream si aspetta una risposta intera")
        }
    }
}

#[tokio::test]
async fn il_conto_viene_saldata_sul_consumo_reale_e_non_sulla_stima() {
    let (gw, meter, tetto) = gateway(
        condiviso(Finto::nuovo("a").con_comportamenti(vec![risposta_con_usage()])),
        10.0,
        &["gpt-4o-mini"],
    );
    gw.gestisci(richiesta()).await;

    // la stima prenotava input stimato + 4096 di output: molto più di quanto è
    // costato. La differenza deve tornare indietro, altrimenti il tetto si consuma
    // da solo e il cliente si vede chiedere il resto del mese dopo poche richieste
    let atteso = prezzi().resolve("gpt-4o-mini").cost(1_000, 500);
    assert_eq!(meter.speso("acme"), atteso);
    assert_eq!(tetto.held(GEN), 0, "la prenotazione non resta mai appesa");
    assert_eq!(tetto.spent(GEN), atteso);
}

// --- autenticazione ---------------------------------------------------------------

#[tokio::test]
async fn senza_chiave_non_si_chiede_niente_a_nessun_provider() {
    let finto = Arc::new(Finto::nuovo("a"));
    let (gw, _, _) = gateway(finto.clone(), 10.0, &["gpt-4o-mini"]);

    let r = gw
        .gestisci(Richiesta {
            chiave: None,
            body: richiesta().body,
        })
        .await;
    assert_eq!(r.status(), 401);
    assert_eq!(
        finto.chiamate(),
        0,
        "una richiesta non autenticata non chiama nessuno"
    );
}

#[tokio::test]
async fn una_chiave_sconosciuta_e_un_401_come_una_manco() {
    // la differenza fra i due casi è nel messaggio al chiamante, non nel comportamento
    let (gw, _, _) = gateway(condiviso(Finto::nuovo("a")), 10.0, &["gpt-4o-mini"]);
    let r = gw
        .gestisci(Richiesta {
            chiave: Some("sk-ignota".to_owned()),
            body: richiesta().body,
        })
        .await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn un_modello_non_autorizzato_per_il_tenant_riceve_403() {
    let (gw, meter, _) = gateway(condiviso(Finto::nuovo("a")), 10.0, &["altro-modello"]);
    let r = gw.gestisci(richiesta()).await;
    assert_eq!(r.status(), 403);
    assert_eq!(
        meter.spesi_negati("acme"),
        1,
        "un 403 è un rifiuto, e va contato come tale"
    );
}

// --- il tetto: la parte che distingue un gateway da un proxy ------------------------

#[tokio::test]
async fn un_tenant_oltre_tetto_riceve_429_e_il_provider_non_viene_chiamato() {
    let finto = Arc::new(Finto::nuovo("a"));
    let (gw, meter, _) = gateway(finto.clone(), 0.000_002, &["gpt-4o-mini"]);

    let r = gw.gestisci(richiesta()).await;
    assert_eq!(r.status(), 429);
    assert_eq!(
        finto.chiamate(),
        0,
        "il punto di tutto: nessun provider è stato chiamato, nessun denaro è uscito"
    );
    assert_eq!(meter.spesi_negati("acme"), 1);
}

#[tokio::test]
async fn il_tetto_che_scade_prima_di_esaurirsi_e_perche() {
    // I numeri, dichiarati perché il test valga quanto dice:
    //   stima   = 9 token stimati × 150 + 4096 output × 600 ≈ 3 µUSD (per eccesso)
    //   consumo = 1000 × 150 + 500 × 600 = 0,45 µUSD → 1 µUSD (per eccesso)
    //   tetto   = 7 µUSD
    // Se il tetto si consumasse della **stima**, 7/3 = due richieste e poi basta.
    // Poiché si salda sul consumo reale, ne passano cinque: è tutta la differenza fra
    // una prenotazione e un addebito.
    let finto = Arc::new(Finto::nuovo("a").con_comportamenti(vec![risposta_con_usage()]));
    let (gw, _, _) = gateway(finto.clone(), 0.000_007, &["gpt-4o-mini"]);

    for passo in 1..=5 {
        assert_eq!(
            gw.gestisci(richiesta()).await.status(),
            200,
            "richiesta {passo}: la stima non deve consumare il tetto"
        );
    }
    assert_eq!(
        gw.gestisci(richiesta()).await.status(),
        429,
        "a questo punto il tetto è davvero esaurito"
    );
    assert_eq!(
        finto.chiamate(),
        5,
        "la sesta richiesta non deve essere arrivata al provider"
    );
}

#[tokio::test]
async fn un_tetto_che_non_e_leggibile_non_ferma_il_cliente() {
    // è la prova che ADR 0004 vale anche per il tetto: se il contabile è rotto,
    // l'utente riceve comunque la sua risposta. Il tetto in quel caso non protegge,
    // e il gateway lo dice nel log — ma non nega il servizio a chi ha fatto bene
    let finto = Arc::new(Finto::nuovo("a").con_comportamenti(vec![risposta_con_usage()]));
    let (gw, meter, tetto) = gateway(finto.clone(), 10.0, &["gpt-4o-mini"]);

    // avvelena il lock come farebbe un panic in un altro thread. Va catturato: il
    // panic è il modo in cui un `Mutex` si avvelena, non un'eccezione che si propaga
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_info| {}));
    let esito = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tetto.avvelena()));
    std::panic::set_hook(hook);

    assert!(
        esito.is_err(),
        "il panic è il modo in cui un Mutex si avvelena: senza, il lock sarebbe intatto"
    );

    let r = gw.gestisci(richiesta()).await;
    assert_eq!(
        r.status(),
        200,
        "una risposta pronta non si butta per un tetto rotto"
    );
    assert_eq!(finto.chiamate(), 1);
    assert!(
        meter.servite_senza_tetto() > 0,
        "il degrado non è silenzioso: il contatore di 'servite senza tetto' sale"
    );
}

// --- il failover attraversa il gateway ---------------------------------------------

#[tokio::test]
async fn un_provider_che_cade_produce_502_e_il_cliente_riceve_un_messaggio_generico() {
    // il client non deve sapere che esiste un secondo provider: quella informazione
    // è inutile per lui e utile a chi volesse mappare l'infrastruttura
    let (gw, meter, tetto) = gateway(
        condiviso(Finto::nuovo("a").con_comportamenti(vec![Comportamento::non_disponibile()])),
        10.0,
        &["gpt-4o-mini"],
    );
    let _ = &meter;

    let r = gw.gestisci(richiesta()).await;
    assert_eq!(r.status(), 502);
    assert_eq!(
        tetto.held(GEN),
        0,
        "una richiesta mai servita non deve bloccare denaro"
    );
    assert_eq!(tetto.spent(GEN), 0);
    assert!(meter.snapshot_di("acme").is_some());
}

#[tokio::test]
async fn un_400_del_provider_torna_al_client_com_e_le_suo_corpo() {
    let (gw, _, _) = gateway(
        condiviso(
            Finto::nuovo("a").con_comportamenti(vec![Comportamento::Risponde(
                400,
                r#"{"error":{"message":"unknown model"}}"#.to_owned(),
            )]),
        ),
        10.0,
        &["gpt-4o-mini"],
    );

    let r = gw.gestisci(richiesta()).await;
    assert_eq!(r.status(), 400, "il suo errore resta il suo errore");
    match r {
        llmgateway::gateway::Risposta::Intera { body, .. } => {
            assert!(String::from_utf8(body).unwrap().contains("unknown model"));
        }
        llmgateway::gateway::Risposta::Flusso { .. } => panic!("una risposta intera"),
    }
}

// --- lo streaming ------------------------------------------------------------------

#[tokio::test]
async fn lo_streaming_passa_davanti_in_chunk_e_resta_dichiarato() {
    let (gw, _, _) = gateway(condiviso(Finto::nuovo("a")), 10.0, &["gpt-4o-mini"]);

    let r = gw
        .gestisci(Richiesta {
            chiave: Some("sk-acme".to_owned()),
            body: br#"{"model":"gpt-4o-mini","stream":true}"#.to_vec(),
        })
        .await;

    match r {
        llmgateway::gateway::Risposta::Flusso {
            status, provider, ..
        } => {
            assert_eq!(status, 200);
            assert_eq!(provider, "a");
        }
        llmgateway::gateway::Risposta::Intera { .. } => {
            panic!("lo streaming non deve diventare una risposta intera: si perde il senso")
        }
    }
}

// --- il caso degenere ---------------------------------------------------------------

#[tokio::test]
async fn un_body_senza_modello_non_e_un_500_del_gateway() {
    // il gateway non giudica il body: lascia che sia il provider a rispondere
    let (gw, _, _) = gateway(condiviso(Finto::nuovo("a")), 10.0, &["gpt-4o-mini"]);
    let r = gw
        .gestisci(Richiesta {
            chiave: Some("sk-acme".to_owned()),
            body: "non è json".as_bytes().to_vec(),
        })
        .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn una_risposta_senza_usage_contabilizza_zero_e_lo_dice() {
    // il gateway non stima a posteriori: un numero inventato è peggio di uno zero
    let (gw, meter, _) = gateway(
        condiviso(Finto::nuovo("a").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")])),
        10.0,
        &["gpt-4o-mini"],
    );
    let r = gw.gestisci(richiesta()).await;
    assert_eq!(r.status(), 200);
    assert_eq!(meter.speso("acme"), 0);
}

#[tokio::test]
async fn il_debug_di_una_risposta_non_stampa_il_contenuto() {
    let (gw, _, _) = gateway(
        condiviso(Finto::nuovo("a").con_comportamenti(vec![risposta_con_usage()])),
        10.0,
        &["gpt-4o-mini"],
    );
    let testo = format!("{:?}", gw.gestisci(richiesta()).await);
    assert!(
        !testo.contains("ciao"),
        "il corpo del provider non finisce in un Debug"
    );
    assert!(testo.contains("status: 200"));
}
