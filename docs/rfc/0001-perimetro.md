# RFC 0001 — Il perimetro di `llmgateway`

> Prima di scrivere una riga di codice: di cosa si parla, cosa no, e perché.
> Stato: **accettato**. Le decisioni tecniche che ne discendono stanno in `../adr/`.

## Il problema

Un'applicazione che chiama un provider LLM in produzione ha tre bisogni che il
provider non copre, e che diventano urgenti tutti insieme al primo cliente pagante.

**1. Quanto costa.** I listini sono per milione di token, per modello, con prezzi diversi
per input e output. L'unico dato che l'API restituisce è l'uso **dell'ultima richiesta**.
Da lì a "quanto ha speso questo tenant ad oggi" c'è un foglio di calcolo che qualcuno
deve aggiornare, e che sbaglia.

**2. Non superare il budget.** Un runaway in un agente — un loop che chiama il provider
troppe volte, un retry che si auto-alimenta, un bug di rientro — puòBruciare un
giorno di fatturazione prima che qualcuno se ne accorga. Un tetto che si verifica
**dopo** la chiamata protegge la richiesta precedente, non quella che sta spendendo.

**3. Non cadere quando un provider cade.** Un 429 o un 503 da un provider è un fatto
accaduto, non un'eccezione: va gestito come una normale giornata di lavoro, con un
altro provider che subentra e senza che l'utente finale se ne accorga.

Il punto comune dei tre è che sono **incroci**: un tenant, un momento nel tempo, un
modello. È esattamente il lavoro che nessuna libreria fa bene e che ogni team riscrive.

## Cosa fa (v0.1)

- **Reverse proxy** di `POST /v1/chat/completions`, compatibile con OpenAI.
- **Tenant** identificato da chiave API: ogni richiesta è attribuita a qualcuno.
- **Budget per tenant** (mensile, in micro-dollari interi) verificato **prima** della
  chiamata, con prenotazione come in `agentloop`.
- **Metering**: il consumo reale della risposta, attribuito a tenant e modello.
- **Failover** su una lista ordinata di provider, per soli errori ritentabili.
- **Streaming** in pass-through: gli chunk SSE arrivano al client quando arrivano,
  senza buffering.
- **Metriche Prometheus** su `/metrics`, testo, senza dipendenze.
- **Log strutturati** JSON su una riga.

## Cosa NON fa (v0.1)

| Non fa | Perché no |
|---|---|
| Cache dei prompt | utile, ma è un'altra politica (ttl, chiave, invalidazione) e va dopo che il resto è verde |
| Rate limiting per RPS | il budget per tenant è il limite che serve; il rate limit è un secondo controllo con una seconda semantica |
| Protocolli non OpenAI | Anthropic ha forme diverse per streaming e usage: sono un adattatore per provider, non una modifica al gateway |
| Autenticazione oltre le chiavi statiche | OAuth, mTLS, rotazione: è un problema di identità, non di gateway |
| Persistenza | lo stato è in memoria. Su più istanze ogni istanza ha il proprio conteggio: va detto (ADR 0006), non nascosto |
| Reload della configurazione a caldo | si riavvia. Un ricaricamento a metà traffico è un caso di coerenza che non vale una versione |

## Il contratto di successo

- [ ] un tenant oltre budget riceve `429` **senza che il provider venga chiamato**
- [ ] un `429` o un `503` dal primo provider finisce al secondo, e il client non lo sente
- [ ] un `400` del provider **non** viene ritentato: è colpa del client, non del provider
- [ ] lo streaming non viene bufferizzato: il primo chunk esce prima che l'upstream abbia finito
- [ ] le metriche contano richieste, errori e costo per tenant e per modello
- [ ] nessun test tocca la rete: la suite gira in meno di 30 secondi
- [ ] `cargo clippy -- -D warnings` verde, `cargo fmt` pulito

## Alternative scartate

- **Usare un gateway gestito** (Helicone, Portkey, LiteLLM in hosting). Esatto il
  bisogno, zero controllo. E il punto di questo progetto è dimostrare di saperlo
  costruire, non saperlo comprare.
- **Farlo in Go**. Il linguaggio migliore sarebbe stato Go: rete, processi, nessuna
  contorta. Scelto Rust per una ragione precisa e dichiarata — il codice di un proxy
  è pieno di `Result`, di stato condiviso e di trasformazioni tra formati, ed è
  lì che i borrow checker costringono a scelte che in Go si farebbero a caso.
  Se il progetto si rivelasse rete semplice, la scelta va rivista: è un fine.
- **Estendere `agentloop` con un modulo di proxy**. agentloop è una libreria e deve
  restarlo. Un gateway è un processo, ha un server, ha stato. Separare è giusto.

## Domande aperte

- **Un tenant come viene identificato?** Ho scelto una chiave statica in
  `Authorization: Bearer`, mappata su un tenant in configurazione. Va bene finché i
  tenant sono pochi e noti. Con tenant che si autogestiscono serve altro: se ne parla
  quando serve.
- **La prenotazione quanto è grande?** Il costo della richiesta non si conosce prima
  di farla. Uso una stima per passo (input stimato + output massimo dal body), che è
  pessimista ma esauriente: vedi ADR 0002.
- **Il metering blocca la risposta?** No: è locale e in memoria, ma un errore nel
  metering non deve mai far perdere una risposta già pronta. Vedi ADR 0004.