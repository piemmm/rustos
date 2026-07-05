## NAME

reset — riportare il terminale a uno stato sano

## SYNOPSIS

`reset`

## DESCRIPTION

Annulla lo stato che un programma a schermo intero bloccato può
lasciare dietro di sé. Prima la disciplina di ingresso torna al valore
interattivo predefinito (i caratteri digitati tornano visibili). Poi
viene scritta la sequenza di ripristino: uscire dallo schermo
alternativo, mostrare il cursore, azzerare colori e attributi,
azzerare la regione di scorrimento e infine portare il cursore
nell'angolo in alto a sinistra e cancellare lo schermo.

Le operazioni emesse dipendono dal terminale indicato in `TERM`;
un'operazione che il terminale non comprende viene omessa. Un terminale
privo di controlli (un `TERM` sconosciuto degrada al profilo minimo)
riceve solo il ripristino della disciplina di ingresso.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `reset` — ripristinare il terminale dopo il blocco di un programma a
  schermo intero.

## EXIT STATUS

- `0` — il terminale è stato ripristinato.
- `1` — l'output non è stato consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `TERM` — il terminale la cui sequenza di ripristino viene scritta.
- `LANG` — la locale preferita per la guida breve (un'etichetta BCP-47
  come `it-IT`).

## SEE ALSO

- `clear`
- `man`
