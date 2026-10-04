# ADR 0001 — Le dipendenze: un set piccolo e dichiarato, non zero

- **Stato:** accettata
- **Data:** 2026-10-04
- **Decide:** Paolo Valletta

## Contesto

`agentloop` ha zero dipendenze runtime, e la cosa è stata possibile perché è una
libreria: il suo unico contratto sono tipi e funzioni, e ogni riga di parsing la si
scrive a mano in un pomeriggio.

Un gateway è diverso: è un processo che sta in ascolto, accetta connessioni, fa
chiamate HTTP a terzi e deve gestire streaming. Ricostruire questo senza librerie
significa:

- un `TcpListener` con parsing HTTP a mano, inclusi chunked encoding e keep-alive;
- un pool di connessioni con timeout e retry, scritto da zero;
- un runtime asincrono, perché una chiamata lenta non deve occupare un thread;
- un parser di SSE;
- un parser TOML.

Nessuno di questi è un problema interessante: sono problemi risolti, con molte
insidie note, da persone che ci hanno passato anni. Il valore del progetto è nel
**budget, nel failover e nel metering**, non nel fatto di aver riscritto HTTP.

## Decisione

Un set minimo di dipendenze, tutte dell'ecosistema Rust "ufficiale":

| Crate | Perché questo e non un altro |
|---|---|
| `tokio` | il runtime asincrono di fatto; `axum` non funziona senza |
| `axum` | routing ed estrattori su `tower`, ed è l'interfaccia del server HTTP standard |
| `reqwest` | client HTTP con supporto streaming e pool di connessioni |
| `serde` + `serde_json` | serializzazione; non è negoziabile in un progetto di questo tipo |
| `toml` | configurazione dichiarata e validata all'avvio |
| `thiserror` | errori tipizzati, uno per riga, senza macro che si espandono in debug |
| `tracing` + `tracing-subscriber` | log strutturato su sink; un `println!` non è un log |

Niente altro. In particolare:

- **niente `serde_yaml`**: TOML esce dalla discussione sul formato ed è più adatto a
  una configurazione con una sezione `[[provider]]` ripetibile;
- **niente crate per Prometheus**: l'esposizione è testo e si scrive in trenta righe;
  aggiungere `prometheus` significa aggiungere metriche a processo, code di raccolta e
  una dipendenza transitiva, per un formattore;
- **niente framework di configurazione**: la validazione è una funzione che restituisce
  una lista di errori leggibili, ed è più utile di un framework.

## Alternative

| Opzione | Pro | Contro | Perché no |
|---|---|---|---|
| std + implementazione propria | zero CVE, zero supply chain | 1500 righe di HTTP/async prima di scrivere un euro di logica | è un progetto diverso, e meno interessante |
| `hyper` invece di `axum` | un livello meno | routing ed estrattori da scrivere a mano | `axum` è sottile sopra `hyper`: non aggiunge un livello, lo nasconde |
| `prometheus` crate | metriche pronte | raccolta, code, feature flag per un formattatore di testo | per il testo di `/metrics` è sovradimensionato |
| Go | rete e concorrenza più semplici | — | scelta deliberata: vedi RFC. È l'unica cosa che ho deciso di non fare e va rivista se il progetto si semplifica |

## Conseguenze

**Si vince:**
- build riproducibile (`Cargo.lock` committato) e superficie di supply chain piccola e
  ispezionabile;
- ogni riga che potevo scrivere in trenta minuti la ho spesa sul budget e sul failover.

**Si paga:**
- aggiornamenti di sicurezza periodici. Con `cargo audit`/`dependabot` sono un'abitudine,
  non un evento;
- il tempo di compilazione iniziale è alto, e su Windows linka in modo indolente. Nessuno
  dei due è un problema di progetto.

## Verifica

Se il grafo delle dipendenze dirette supera le sette righe della tabella sopra, o se
`cargo tree` mostra che una dipendenza diretta ne porta tre di cui una non serve, la
decisione va rivista: significa che la libreria si sta portando dentro delle
responsabilità nostre.