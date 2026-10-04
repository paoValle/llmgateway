# ADR 0000 — Come scriviamo le decisioni

Le decisioni tecniche che *non* sono ovvie si registrano qui, una per file.
Obiettivo: fra due anni capire **perché** il codice è fatto così, non **cosa** fa.

Il formato è quello di Michael Nygard, leggero: il resto del documento nasce solo se serve.

## Quando serve un ADR

Scrivi un ADR se, rileggendo la diff, qualcuno potrebbe chiederti *"ma perché non hai
fatto X?"*. In pratica:

- scelta di una dipendenza (o del suo rifiuto)
- algoritmo, struttura dati, formato di serializzazione
- confine di modulo, dove finisce una responsabilità
- scelta che lega il progetto a un servizio esterno
- prestazione: un compromesso deliberato (memoria vs CPU, latenza vs costo)

**Non** serve un ADR per: nomi di funzioni, formattazione, bug fix ovvi, scelte che
un senior farebbe uguale.

## Formato del file

`docs/adr/NNNN-titolo-in-kebab-case.md`, numerazione progressiva mai riutilizzata.
Le ADR sono immutabili: se la decisione cambia, ne scrivi una nuova che le sostituisce.

```markdown
# ADR NNNN — Titolo

- **Stato:** proposta | accettata | superata da [NNNN]
- **Data:** YYYY-MM-DD
- **Decide:** Paolo Valletta

## Contesto
Quali forze premono. Fatti, non opinioni. Se ci sono numeri, qui.

## Decisione
Cosa decidiamo, in una frase all'attivo. "Useremo X", non "è stato scelto X".

## Alternative
| Opzione | Pro | Contro | Perché no |
|---|---|---|---|
| A | | | |
| B | | | |

## Conseguenze
Cosa diventa possibile, cosa diventa impossibile, cosa ci siamo esposti.
Le cose negative contano più delle positive.

## Verifica
Come facciamo a sapere se la decisione era giusta? Quale misura, entro quale data.
```

## Il numero 0000

Questo file non è una decisione, è la regola. Non lo rinumerare.