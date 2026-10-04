# ADR 0006 — Lo stato vive in memoria, e una sola istanza è l'unico modo di crederci

- **Stato:** accettata
- **Data:** 2026-10-04
- **Decide:** Paolo Valletta

## Contesto

I contatori di un gateway sono per definizione per tenant e per finestra temporale. Il
tenant lo si sa dalla chiave API; la finestra la si sa dall'orologio. Entrambe le cose
portano a uno stato che deve esistere da qualche parte.

Le tre opzioni sono: in memoria, su disco, in un database.

Il database è la risposta giusta in generale e la risposta sbagliata qui, per una
ragione precisa: **il numero di richieste che il gateway gestisce è già il numero di
richieste che il provider sta gestendo**. Nessuno mette un Postgres davanti a un API
che non lo chiede. Ma il punto è un altro: se lo stato è su disco o in un database, il
gateway ha acquistato un modo di perdere i dati (disco pieno, connessione caduta) che
non aveva.

In memoria, la perdita è solo in caso di crash, ed è **limitata all'ultima finestra**:
al riavvio riparti da zero per il tenant, e il tetto mensile riparte con lui. Questo
non è un dettaglio, è un buco nella garanzia.

## Decisione

Stato in memoria, in processi di copertura più ampia di quanto il bilancio di ADR 0001
vorrebbe, e **la singola istanza è un requisito dichiarato**, non un caveat.

- `v0.1` è progettato e documentato per **una sola istanza**. Con due, ogni istanza ha
  il proprio conteggio e un tenant che passa da una all'altra vede il tetto dimezzato.
- Non si mette un lock distribuito per porre rimedio: costerebbe una dipendenza e una
  latenza su ogni richiesta per coprire un caso che nella configurazione d'uso non
  esiste.
- Se servono più istanze, la strada è un backend del contatore (Redis, o lo stesso
  provider come fonte autorevole) e **un pezzo in più per il tetto**: serve una
  prenotazione distribuita, e ADR 0002 la fa diventare un protocollo, non una riga.

In concreto, il README dice:

> **Una sola istanza.** Con più di un processo, ogni istanza conta per sé e il tetto per
> tenant non è più un tetto. Questo progetto è un side project: dichiarare il limite
> vale più che metterci sopra un lock distribuito che non sapresti gestire.

E il codice lo rende visibile: lo stato espone il numero di richieste servite, così un
operatore che vede il tetto saltare sa che ha più istanti prima di sospettare un bug.

## Alternative

| Opzione | Pro | Contro | Perché no |
|---|---|---|---|
| solo in memoria | semplice, veloce, nessuna dipendenza | il conteggio muore al riavvio | è dichiarato, e il buco è limitato alla finestra corrente |
| su disco (file append) | sopravvive al riavvio | I/O e fsync nel percorso critico, o un writer in background che è una seconda coda | complica il failover del percorso critico |
| SQLite | semplice, sopravvive | scritture con lock, stessi problemi del disco | idem, con un formato che finge di essere un database |
| Redis | multi-istanza, veloce | dipendenza operativa, e la prenotazione diventa distribuita | la risposta giusta al problema sbagliato per questa versione |

## Conseguenze

**Si vince:**
- nessuna dipendenza operativa oltre il binario;
- il percorso critico non tocca disco né rete oltre l'upstream.

**Si perde:**
- al riavvio i contatori ripartono da zero;
- **più istanze rompono il tetto.** È il limite più importante del progetto ed è scritto
  nel README, non nascosto in un dettaglio.

## Verifica

Se il progetto dovesse accogliere utenti reali, il primo cambiamento da fare è un
backend del contatore. Il test che lo rende necessario: due istanze, un tenant, un
tetto — e il tetto che non scatta. Quel test non c'è ora, perché una sola istanza è
un requisito e non un bug.