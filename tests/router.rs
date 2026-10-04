//! Il failover, verificato senza toccare la rete.
//!
//! Le tre classi di ADR 0003 hanno ciascuna almeno un test. Se una delle tre non è
//! coperta, la tabella dell'ADR è solo documentazione.

mod support;

use std::time::Duration;

use llmgateway::router::{RouteError, Router, DEFAULT_MAX_ATTEMPTS};
use llmgateway::upstream::{ResponseBody, TransportKind, UpstreamRequest};

use support::{condiviso, Comportamento, Finto};

const TIMEOUT: Duration = Duration::from_secs(5);

fn richiesta(modello: &str) -> llmgateway::upstream::UpstreamRequest {
    UpstreamRequest::new(br#"{"messages":[]}"#.to_vec(), modello, false)
}

fn router(provider: Vec<std::sync::Arc<dyn llmgateway::upstream::Upstream>>, max: usize) -> Router {
    Router::new(provider, max, TIMEOUT)
}

#[tokio::test]
async fn il_primo_provider_che_risponde_vince_e_gli_altri_non_si_accendono() {
    let a =
        condiviso(Finto::nuovo("a").con_comportamenti(vec![Comportamento::ok("{\"da\":\"a\"}")]));
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"da\":\"b\"}")]));
    let r = router(vec![a, b], 4)
        .route(richiesta("gpt-4o-mini"))
        .await
        .expect("servito");

    assert_eq!(r.provider, "a");
    assert_eq!(r.attempts, 1);
    match r.response.body {
        ResponseBody::Buffered(b) => assert_eq!(String::from_utf8(b).unwrap(), "{\"da\":\"a\"}"),
        ResponseBody::Stream(_) => {
            panic!("una risposta non richiesta come streaming non lo deve essere")
        }
    }
}

// --- classe 1: errore del provider, si ritenta -------------------------------------

#[tokio::test]
async fn un_503_passa_al_provider_successivo_e_il_cliente_non_se_ne_accorge() {
    let a = condiviso(Finto::nuovo("a").con_comportamenti(vec![Comportamento::non_disponibile()]));
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    let r = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect("il secondo deve rispondere");

    assert_eq!(r.provider, "b");
    assert_eq!(r.attempts, 2, "il primo tentativo è stato contato");
}

#[tokio::test]
async fn un_429_passa_al_provider_successivo_anche_se_è_un_4xx() {
    let a = condiviso(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::Risponde(
            429,
            "{\"e\":\"rate\"}".into(),
        )]),
    );
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    let r = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect("il secondo deve rispondere");
    assert_eq!(r.provider, "b");
}

#[tokio::test]
async fn una_connessione_rifiutata_prosegue_verso_il_successivo() {
    let a = condiviso(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::NonEsce(TransportKind::Connect)]),
    );
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    let r = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect("il secondo deve rispondere");
    assert_eq!(r.provider, "b");
}

// --- classe 2: errore del cliente, nessun failover ---------------------------------

#[tokio::test]
async fn un_400_non_è_ritentato_su_nessun_provider() {
    let a = condiviso(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::Risponde(
            400,
            "{\"error\":\"bad request\"}".into(),
        )]),
    );
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));
    let errore = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect_err("un 400 va al client");

    match errore {
        RouteError::ClientFault {
            provider,
            status,
            body,
        } => {
            assert_eq!(provider, "a");
            assert_eq!(status, 400);
            assert!(
                String::from_utf8(body).unwrap().contains("bad request"),
                "il corpo va inoltrato"
            );
        }
        altro => panic!("errore inatteso: {altro:?}"),
    }
}

#[tokio::test]
async fn un_400_su_tutti_i_provider_produce_una_risposta_e_non_un_esaurimento() {
    // il client ha sbagliato: dirgli "abbiamo provato tre provider" non lo aiuta,
    // e far consumare tre tentativi a un errore suo è tempo sprecato
    let a = condiviso(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::Risponde(400, "{\"e\":1}".into())]),
    );
    let b = condiviso(
        Finto::nuovo("b").con_comportamenti(vec![Comportamento::Risponde(400, "{\"e\":2}".into())]),
    );

    let errore = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect_err("400 ovunque");
    assert!(
        matches!(errore, RouteError::ClientFault { .. }),
        "{errore:?}"
    );
}

// --- classe 3: stato sconosciuto, nessun failover ---------------------------------

#[tokio::test]
async fn uno_status_non_classificato_ferma_il_failover() {
    // un 301 da un provider di chat completions è qualcosa che il gateway non sa
    // interpretare. Amplificarlo su tre provider sarebbe peggio che propagarlo:
    // l'errore resta, ma con la sua causa e non come un "provider non risponde"
    let a = condiviso(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::Risponde(301, "moved".into())]),
    );
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    let errore = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect_err("301 non si ritenta");
    match errore {
        RouteError::UnknownStatus {
            provider, status, ..
        } => {
            assert_eq!(provider, "a");
            assert_eq!(status, 301);
        }
        altro => panic!("errore inatteso: {altro:?}"),
    }
}

// --- la regola che vale più di tutte: niente doppio addebito -----------------------

#[tokio::test]
async fn una_richiesta_già_partita_non_è_ritentata_anche_se_il_provider_sembra_sano() {
    // il provider è andato via dopo aver ricevuto la richiesta: potrebbe averla
    // eseguita. Riprovare su un altro provider può farla eseguire due volte.
    let a =
        condiviso(
            Finto::nuovo("a").con_comportamenti(vec![Comportamento::PartitaSenzaRisposta(
                TransportKind::Reset,
            )]),
        );
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    let errore = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect_err("niente failover");

    match errore {
        RouteError::DeliveryUnknown { provider, error } => {
            assert_eq!(provider, "a");
            assert_eq!(error.kind, TransportKind::Reset);
            assert!(!error.delivery.safe_to_retry());
        }
        altro => panic!("errore inatteso: {altro:?}"),
    }
}

// --- il tetto di tentativi ---------------------------------------------------------

#[tokio::test]
async fn il_tetto_di_tentativi_su_tutti_i_provider_ferma_il_failover() {
    let a = condiviso(Finto::nuovo("a").con_comportamenti(vec![Comportamento::non_disponibile()]));
    let b = condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::non_disponibile()]));
    let c = condiviso(Finto::nuovo("c").con_comportamenti(vec![Comportamento::non_disponibile()]));
    let d =
        condiviso(Finto::nuovo("d").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    // con tetto 2, il quarto provider non deve mai essere chiamato
    let errore = router(vec![a, b, c, d], 2)
        .route(richiesta("m"))
        .await
        .expect_err("esaurito");

    match errore {
        RouteError::Exhausted { attempts, .. } => assert_eq!(attempts, 2),
        altro => panic!("errore inatteso: {altro:?}"),
    }
}

#[tokio::test]
async fn un_tetto_di_zero_non_significa_nessun_tentativo() {
    // zero tentativi sarebbe un gateway che non serve nessuno: il default è 4
    let a =
        condiviso(Finto::nuovo("a").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));
    let r = router(vec![a], 0);
    assert_eq!(r.max_attempts(), DEFAULT_MAX_ATTEMPTS);
    assert!(r.route(richiesta("m")).await.is_ok());
}

// --- il routing per modello ---------------------------------------------------------

#[tokio::test]
async fn un_provider_che_non_serve_il_modello_viene_saltato_e_non_è_un_errore() {
    // non è un fallimento: è una scelta di routing. Contarlo come errore farebbe
    // urlare allarme ogni volta che si cambia modello
    let solo_a = condiviso(Finto::con_modelli("solo-a", &["modello-a"]));
    let solo_b = condiviso(Finto::con_modelli("solo-b", &["modello-b"]));

    let r = router(vec![solo_a, solo_b], 4)
        .route(richiesta("modello-b"))
        .await
        .expect("modello-b");

    assert_eq!(r.provider, "solo-b");
    assert_eq!(r.attempts, 1, "solo-b ha ricevuto una richiesta");
}

#[tokio::test]
async fn un_modello_che_nessuno_serve_dice_che_cosa_serve_e_cosa_no() {
    let solo_a = condiviso(Finto::con_modelli("solo-a", &["modello-a"]));

    let errore = router(vec![solo_a], 4)
        .route(richiesta("modello-z"))
        .await
        .expect_err("nessuno lo serve");

    match errore {
        RouteError::NoProviderForModel { model, servibili } => {
            assert_eq!(model, "modello-z");
            assert_eq!(servibili, vec!["modello-a".to_owned()]);
        }
        altro => panic!("errore inatteso: {altro:?}"),
    }
}

#[tokio::test]
async fn un_provider_che_serve_tutti_i_modelli_non_ha_una_lista_da_manutenere() {
    let generico = condiviso(Finto::nuovo("generico"));
    assert!(router(vec![generico], 4)
        .route(richiesta("qualsiasi"))
        .await
        .is_ok());
}

#[tokio::test]
async fn senza_provider_il_router_lo_dice_e_non_prova_nulla() {
    let errore = router(vec![], 4)
        .route(richiesta("m"))
        .await
        .expect_err("nessun provider");
    assert!(matches!(errore, RouteError::NoProviders));
}

// --- lo streaming ------------------------------------------------------------------

#[tokio::test]
async fn lo_streaming_arriva_in_chunk_e_non_come_un_corpo_unico() {
    let a = condiviso(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::Risponde(
            200,
            "{\"chunk\":1}\n{\"chunk\":2}\n{\"chunk\":3}".to_owned(),
        )]),
    );

    let r = router(vec![a], 4)
        .route(UpstreamRequest::new(b"{}".to_vec(), "m", true))
        .await
        .expect("streaming servito");

    assert!(
        r.response.body.is_stream(),
        "lo streaming non deve diventare un body in memoria"
    );
}

// --- l'ordine dei provider ----------------------------------------------------------

#[tokio::test]
async fn i_provider_vengono_provati_nell_ordine_in_cui_sono_dati() {
    let a = condiviso(Finto::nuovo("a").con_comportamenti(vec![Comportamento::non_disponibile()]));
    let b = condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::non_disponibile()]));
    let c =
        condiviso(Finto::nuovo("c").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    let r = router(vec![a, b, c], 5)
        .route(richiesta("m"))
        .await
        .expect("servito da c");
    assert_eq!(r.provider, "c");
    assert_eq!(r.attempts, 3);
}

#[tokio::test]
async fn il_conteggio_delle_chiamate_racconta_dove_sono_stati_i_tentativi() {
    let a = std::sync::Arc::new(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::non_disponibile()]),
    );
    let b = std::sync::Arc::new(
        Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]),
    );
    let r = router(vec![a.clone(), b.clone()], 4);

    let risposta = r.route(richiesta("m")).await.expect("servito da b");
    assert_eq!(risposta.provider, "b");

    // il primo provider è stato chiamato una volta e ha fallito: contarlo è ciò che
    // distingue "ha provato due provider" da "ha riprovato lo stesso due volte"
    assert_eq!(a.chiamate(), 1);
    assert_eq!(b.chiamate(), 1);
    assert_eq!(a.modelli_richiesti(), vec!["m".to_owned()]);
    assert_eq!(b.modelli_richiesti(), vec!["m".to_owned()]);
}

#[tokio::test]
async fn un_provider_saltato_non_viene_contato_come_chiamato() {
    let a = std::sync::Arc::new(Finto::con_modelli("solo-a", &["altro"]));
    let b = std::sync::Arc::new(Finto::nuovo("b"));
    let r = router(vec![a.clone(), b.clone()], 4);

    r.route(richiesta("m")).await.expect("servito da b");
    assert_eq!(a.chiamate(), 0, "saltare un provider non è chiamarlo");
}

#[tokio::test]
async fn un_4xx_che_il_gateway_non_interpreta_ferma_il_failover_come_ogni_altro_4xx() {
    // 451 è un 4xx: non è uno "status ignoto", è del client come un 400.
    // Il router si ferma in entrambi i casi, che è il punto
    let a = condiviso(
        Finto::nuovo("a").con_comportamenti(vec![Comportamento::Risponde(451, "{\"e\":1}".into())]),
    );
    let b =
        condiviso(Finto::nuovo("b").con_comportamenti(vec![Comportamento::ok("{\"ok\":true}")]));

    let errore = router(vec![a, b], 4)
        .route(richiesta("m"))
        .await
        .expect_err("451 non si ritenta");
    assert!(
        matches!(errore, RouteError::ClientFault { status: 451, .. }),
        "{errore:?}"
    );
}
