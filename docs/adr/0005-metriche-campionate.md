# ADR 0005 — Le metriche sono campionate, e il README lo dice

- **Stato:** accettata
- **Data:** 2026-10-04
- **Decide:** Paolo Valletta

## Contesto

Un gateway Prometheus si misura in tre modi: un contatore incrementato a ogni
richiesta, un timer che registra ogni durata, o entrambi. La forma ovvia è contare
tutto.

Il costo di contare tutto non è il contatore: è la **contesa**. Ogni richiesta che
incrementa un contatore globale prende un lock. Con un solo lock per il processo, il
gateway diventa single-threaded su un percorso che per definizione gira in parallelo.
Su un carico reale, la contesa è visibile in latenza.

La domanda quindi non è "le metriche sono utili" — lo sono — ma "quanto esattezza
serve, e a chi serve".

## Decisione

**Campionamento a contatore globale atomico**, con un errore noto e dichiarato.

Un `AtomicU64` globale, senza lock, con un contatore che avanza a ogni evento. Le
metriche per tenant e per modello vengono aggiornate solo quando il campionamento dice
"questa è una delle N". Niente `Mutex<HashMap<>>` nel percorso caldo.

Cosa si perde, esplicitamente:

- **le metriche per tenant sono stimate**, non esatte: se N = 100, ogni tenant è
  contato con un errore dell'ordine di ±√N/100 sul campione. Va bene per un pannello,
  non va bene per una fattura;
- **le metriche di durata sono campionate**, non un istogramma esatto.

Cosa **non** è campionato, perché serve esatto: **il denaro**. Il totale per tenant è
sempre esatto, perché è un contatore `AtomicU64` per tenant, non una tabella. Il
campionamento vale per le metriche operative (`requests_total`, errori, durate), non
per `spend_micro_usd`.

E la cosa che conta di più, scritta nel README:

> **il dato autorevole del costo è la fattura del provider.** Questo gateway conta per
> darti un controllo continuo e un allarme precoce, non per sostituire la fattura.

Una dashboard che sembra autorevole e non lo è è peggio di nessuna dashboard: fa
prendere decisioni su numeri sbagliati.

## Alternative

| Opzione | Pro | Contro | Perché no |
|---|---|---|---|
| contare tutto con `Mutex<HashMap>` | esatto e semplice | contesa su ogni richiesta; il gateway diventa single-threaded | il costo è nel percorso critico |
| contare tutto con atomici per tenant | esatto, poca contesa | una mappa per tenant comunque: crescita illimitata e lookup per richiesta | il numero di tenant è illimitato, quello delle entry no |
| solo histogram, niente contatori | Costa bassa | i contatori sono la metrica che si guarda per prima | si rinuncia a metà del dato per non pagare l'altra metà |
| contatori esatti in un processo separato | esatto, no contesa nel percorso caldo | un altro processo da gestire, e il dato è comunque asincrono | complessità per un dato che non serve esatto |

## Conseguenze

**Si vince:**
- il percorso caldo non prende lock;
- il costo per tenant resta esatto, che è il numero per cui esiste il gateway.

**Si paga:**
- le metriche operative sono stimate, e va scritto da qualche parte che lo sono.
  "Da qualche parte" significa **nel README e nel nome della metrica**: `sampled_`,
  non `requests_total` che suona esatto.

## Verifica

Il test deve mostrare che il conteggio campionato resta entro l'errore atteso su un
carico noto, e che `spend_micro_usd` è esatto. Se il primo oscilla troppo, `N` è troppo
alto.