# Contributing

Grazie per il tempo. Questo è un side project: la barra è "leggibile fra sei mesi",
non "production-grade". Ma la barra è comunque alta.

## Loop di lavoro

1. Apri un'**issue** che descriva il problema in 5 righe. Se non sai scriverla,
   il problema non è ancora chiaro.
2. Branch: `feat/xyz`, `fix/xyz`, `chore/xyz`.
3. Un commit = un concetto. Messaggi in [Conventional Commits](https://www.conventionalcommits.org/).
4. Apri la PR. Max ~300 righe. La descrizione dice **perché**, non **cosa**: il *cosa* lo dice il diff.
5. `make ci` deve essere verde. Non si mergia il rosso "poi lo sistemo".

## Stile

- Il formatter automatico decide (`.editorconfig` + config del linguaggio).
- Nessun segreto, nessun dato reale, nessun binario committato per errore.
- Se prendi codice o un'idea da fuori: link nell'intestazione del file o nel commit.
- I commenti spiegano il **perché**. Il **cosa** lo dice il codice.
- Le decisioni non ovvie vanno in `docs/adr/`, non in un commento sparso.

## Segnalare problemi

Apri un'issue. Se è un bug, l'ideale è un caso minimo riproducibile.
