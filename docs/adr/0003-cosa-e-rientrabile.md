# ADR 0003 — Si ritenta ciò che è del provider, mai ciò che è del client

- **Stato:** accettata
- **Data:** 2026-10-04
- **Decide:** Paolo Valletta

## Contesto

In un gateway con failover, la domanda è sempre la stessa: quando un provider non va
bene, si passa al successivo? La risposta facile — "sempre" — è sbagliata in metà dei
casi, e lo sbaglio costa due volte: denaro speso e tempo dell'utente.

Un `400 Bad Request` è colpa del **client**: ha mandato una richiesta che nessun
provider accetterà. Riprovare sul provider successivo produce lo stesso `400`, dopo
una latenza che l'utente ha già visto. Peggio: in un gateway che logga, si finisce per
attribuire a un provider un errore che è del cliente.

Un `429 Too Many Requests` o un `503 Service Unavailable` è diverso: è del provider.
Passare al successivo è esattamente il motivo per cui esistono i gateway.

Un caso in-between è il `401`/`403`: chiave non valida o permesso mancante. Ritentare
cambiando provider potrebbe funzionare (l'errore è della chiave su *quel* provider), ma
è una diagnosi che il gateway non può fare: potrebbe essere una chiave scaduta ovunque.
Ritenta solo se un errore è **del provider e riguarda quel provider**.

## Decisione

Una risposta si classifica in tre, e la classificazione decide:

| Classe | Esempi | Azione |
|---|---|---|
| `Rientrabile` | 408, 429, 5xx, errore di trasporto, timeout | passa al provider successivo |
| `Del cliente` | 400, 401, 403, 404, 413, 422 | restituisci al client, **nessun failover** |
| `Sconosciuta` | qualsiasi altro status | **nessun failover**, log di allerta |

Il caso `Sconosciuta` è la scelta che va difesa in review: il riflesso è ritentare
tutto il resto. Ma un `402 Payment Required` o un `451` hanno semantiche che il gateway
non conosce, e su un errore non capito la cosa giusta è **non fare niente** e
farselo notare. Il failover è una comodità; un failover che amplifica un errore
sconosciuto è un disservizio.

In più, due regole che tengono insieme le cose:

- **nessun ritentativo non idempotente**: se la richiesta è stata accettata e non è
  chiaro se è stata eseguita (timeout dopo l'invio), non si passa al provider
  successivo con la stessa richiesta. Un doppio addebito è peggio di un errore;
- **budget di tentativi**: al massimo `len(provider)` tentativi, e la somma dei
  ritentativi di tutti i provider è limitata da una costante di configurazione. Il
  failover che non ha un tetto è un DDoS che si autoalimenta.

## Alternative

| Opzione | Pro | Contro | Perché no |
|---|---|---|---|
| ritentare tutto | codice più semplice | trasforma un 400 dell'utente in 3 secondi di attesa e 3 righe nei log | amplifica l'errore invece di propagarlo |
| ritentare solo 5xx | facile da ricordare | 429 è il caso più comune in assoluto e resterebbe scoperto | esclude proprio il caso che il failover serve a coprire |
| ritentare sullo status di default della libreria HTTP | nessuna decisione | la libreria non sa nulla di tenant, provider e denaro | la decisione è economica e di dominio: è qui che deve stare |

## Conseguenze

**Si vince:**
- i log hanno senso: un errore è attribuito a qualcuno;
- il tempo di risposta dell'utente non cresce per un suo errore;
- i doppi addebiti sono esclusi per costruzione.

**Si paga:**
- un provider che risponde `400` per un suo problema (non per quello del client) non
  viene ritentato, e il client riceve un errore che non era suo. Non abbiamo modo di
  distinguerlo. È il limite accettato di questa scelta.

## Verifica

Un test per ciascuna classe, con un upstream finto che risponde in un modo diverso a
richieste diverse. Se le tre classi non sono tutte coperte, la tabella è solo
documentazione.