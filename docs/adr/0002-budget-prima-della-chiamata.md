# ADR 0002 — Il budget si controlla prima della chiamata, con una stima pessimista

- **Stato:** accettata
- **Data:** 2026-10-04
- **Decide:** Paolo Valletta

## Contesto

Il costo di una chiamata a un provider è `input_tokens × prezzo_input + output_tokens ×
prezzo_output`. I token di input si possono contare prima, perché sono nel body della
richiesta (con un tokenizer, che è un problema serio). Quelli di output **non si
conoscono prima**: dipendono dal modello e dalla domanda.

Il caso facile è quindi la risposta: si guarda `usage` e si fa il conto. Il caso facile
è anche quello che non serve, perché al momento in cui la risposta c'è, il denaro è
andato. Un runaway che fa 4 000 richieste in un'ora non si ferma al centesimo.

## Decisione

**Prenotazione prima, saldo dopo.** Come in `agentloop`, e per la stessa ragione: un
tetto che si verifica dopo protegge la richiesta precedente.

La stima è composta da due parti:

```
stima = token_input_stimati × prezzo_input + output_massimo × prezzo_output
```

- `token_input_stimati`: `lunghezza_del_body / 4`, arrotondata per eccesso. È la
  stima standard per i token, e per eccesso significa che **non si sottovaluta mai**.
  Il divisore è tarato sui tokenizer dei modelli di uso comune: se un tenant invia testo
  italiano o codice il rapporto è più alto, e per eccesso resta un tetto.
- `output_massimo`: letto da `max_tokens` se presente, altrimenti una costante di
  configurazione. **Il default è volutamente alto**: se il client non dice quanto vuole
  generare, si presume il peggio. Sottovalutare qui significa non proteggere.

Se la prenotazione non entra nel budget residuo del tenant, si risponde `429` e **il
provider non viene chiamato**: è il comportamento che distingue un tetto da un conteggio.

Il saldo usa i `usage` reali e libera la differenza. Se la risposta non porta `usage`,
si salda con la stima: è la cosa meno sbagliata da fare quando non si sa.

## Alternative

| Opzione | Pro | Contro | Perché no |
|---|---|---|---|
| controllo solo dopo | esatto, zero falsi positivi | protegge la richiesta precedente | non è un tetto, è un rendiconto |
| contare i token con un tokenizer esatto | stima precisa | serve il tokenizer del modello; un modello nuovo lo invalida | la dipendenza cresce più dell'accuratezza che si guadagna |
| token medi per tenant | zero costo | un tenant con prompt lunghi passa finché non è troppo tardi | sposta il problema, non lo risolve |
| quota in richieste | semplicissimo | un prompt da 200k token costa più di cento da 500 | non misura la risorsa che viene fatturata |

## Conseguenze

**Si vince:**
- il tetto vale davanti alla spesa, e la prenotazione è l'unico meccanismo che lo rende vero;
- nessun rifiuto per tenant che chiede meno del previsto: la stima per eccesso genera
  falsi positivi rari, non frequenti;
- un solo meccanismo in tutto il portfolio (`agentloop` e qui), quindi si spiega una volta.

**Si paga:**
- **falsi positivi**: un tenant con prompt lunghi e `max_tokens` generoso può vedersi
  rifiutato una richiesta che in realtà costerebbe poco. Il rimedio è dichiarare
  `max_tokens`, che è buona igiene dall'altra parte;
- la stima sul divisore 4 è grossolana. Va bene per un tetto, male per un preventivo.

## Verifica

Il test che conta: un tenant a 1 ¢ dal tetto che manda una richiesta da 1 token deve
passare; lo stesso tenant che ne manda una da 100 000 deve ricevere `429` e il provider
finto deve avere ricevuto **zero** richieste.