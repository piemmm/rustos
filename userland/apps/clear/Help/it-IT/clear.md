## NAME

clear — cancellare lo schermo del terminale

## SYNOPSIS

`clear [-x]`

## DESCRIPTION

Scrive la sequenza che porta il cursore nell'angolo in alto a sinistra
e cancella l'intero schermo, lasciandolo vuoto. La sequenza emessa
dipende dal terminale indicato in `TERM`; un terminale che non può
cancellare (un `TERM` sconosciuto degrada al profilo minimo) fa fallire
il comando invece di stampare byte che il terminale mostrerebbe come
caratteri spuri.

Le console TAIRiX non conservano alcuna cronologia di scorrimento,
quindi non c'è nulla da cancellare in quel senso: `-x` (l'opzione GNU
che preserva la cronologia) è accettata per compatibilità con gli
script e non cambia nulla.

## OPTIONS

- `-x` — accettata per compatibilità GNU; una console TAIRiX non
  conserva cronologia, l'output è identico con o senza.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `clear` — cancellare lo schermo.

## EXIT STATUS

- `0` — la sequenza di cancellazione è stata scritta.
- `1` — il terminale non può cancellare, o l'output non è stato
  consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `TERM` — il terminale la cui sequenza di cancellazione viene scritta.
- `LANG` — la locale preferita per la guida breve (un'etichetta BCP-47
  come `it-IT`).

## SEE ALSO

- `reset`
- `man`
