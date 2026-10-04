# ADR 0004 — Il metering non può mai far perdere una risposta

- **Stato:** accettata
- **Data:** 2026-10-04
- **Decide:** Paolo Valletta

## Contesto

Il gateway ha due compiti con la stessa risposta davanti: **consegnarla al client** e
**registrare quanto è costata**. Sono in tensione: il secondo è locale e in memoria, il
primo è ciò che l'utente sta aspettando.

Il modo naturale di scrivere il codice è fare le cose in sequenza: recupera la risposta,
aggiorna i contatori, poi inoltra. Funziona finché l'aggiornamento dei contatori è
lento, o va in errore, o il processo è sotto pressione di memoria.

A quel punto la domanda diventa: **a chi si sacrifica la risposta?** E la risposta
ovvia, "al contatore", è quella sbagliata. Se l'utente non riceve la risposta, l'agente
non funziona, e il ticket che arriva è "il gateway è lento" — non "abbiamo perso un
contatore". Il contatore perso si recupera guardando la fattura del provider. La
risposta persa non si recupera.

C'è anche il caso dell'errore vero: un bug nel metering — un `unwrap` su qualcosa di
imprevisto, un overflow, una chiave mancante — che diventa un 500 per un utente che ha
fatto tutto bene.

## Decisione

**Il contabile non fa fallire il servizio.**

1. **Prima la risposta, poi il conto.** La risposta dell'upstream viene inoltrata al
   client prima che il metering venga aggiornato. Se il metering fallisce, la risposta
   è già partita.
2. **Il metering non ha modo di fallire.** Non c'è `Result`: `record()` ha una firma che
   non ammette fallimenti. Se qualcosa non torna (modello sconosciuto, contatore
   traboccante), la funzione **degrada e lo registra**, con un contatore
   `metering_errors_total` che sale.
3. **Il fallback è pessimista.** Se il modello non è in tabella, si stima con il prezzo
   più alto noto, come in `agentloop`: un conto sbagliato in eccesso è recuperabile, un
   conto sbagliato in difetto è un buco che nessuno nota fino alla fattura.
4. **Il conteggio non blocca.** Un `RwLock` su un contatore globale diventerebbe un
   collo di bottiglia sotto carico. I contatori sono per tenant in una mappa dietro un
   `RwLock` corto, e le metriche sono **approssimate per campionamento** (ADR 0005):
   contare ogni richiesta è più costoso di quanto valga il dato.

## Alternative

| Opzione | Pro | Contro | Perché no |
|---|---|---|---|
| contare in modo esatto e bloccante | il numero è vero | un errore nel contatore uccide la risposta; sotto carico è un collo di bottiglia | ottimizza la metrica a spese del servizio |
| contare in un canale asincrono (mpsc) | la risposta non attende mai | se il canale è pieno i messaggi si perdono, e non c'è modo di saperlo | si sposta la perdita, non la si evita |
| se il metering fallisce, rispondere 500 | il fallimento è visibile | un bug interno diventa un disservizio per l'utente | l'utente non può fare niente per un bug di conteggio |
| salvare su disco a ogni richiesta | sopravvive al riavvio | I/O per richiesta su un percorso critico | la persistenza è fuori perimetro (RFC), e va detta |

## Conseguenze

**Si vince:**
- un errore di conteggio è un contatore e una riga di log, non un disservizio;
- il tempo di risposta non dipende dalla scrittura dei contatori.

**Si perde:**
- **il conteggio può perdere richieste.** È il prezzo dichiarato, e la sua consistenza
  è approssimata per campionamento (ADR 0005). Va detto nel README, perché un utente
  che fa fatture su questi numeri deve saperlo: il dato autorevole resta quello del
  provider.
- se il gateway cade, il conto della finestra corrente muore con lui.

## Verifica

Un test che fa fallire il metering e verifica che il client riceve comunque `200` con il
corpo giusto. Se quel test non esiste, questa ADR è un'intenzione.