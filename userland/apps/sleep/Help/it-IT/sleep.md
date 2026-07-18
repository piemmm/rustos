## NAME

sleep — mettere in pausa per la somma di intervalli di tempo

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

Mette in pausa per la somma degli intervalli indicati e poi termina.

Ogni `NUMBER` è un valore in virgola mobile; un `SUFFIX` di una sola
lettera lo scala: `s` per i secondi (predefinito), `m` per i minuti, `h`
per le ore e `d` per i giorni. Più operandi vengono sommati, quindi
`sleep 1m 30s` mette in pausa per novanta secondi. `inf` (o `infinity`)
mette in pausa finché il processo non viene terminato.

Diversamente dalla temporizzazione propria di una shell, `sleep` dorme
fuori dal processore: il compito viene parcheggiato finché l'intervallo non
è trascorso e non fa mai girare a vuoto un core.

Un valore negativo, un `nan`, un suffisso sconosciuto o caratteri
aggiuntivi dopo il numero è un `invalid time interval`. Non dare alcun
operando è un `missing operand`.

Questo comando non stampa una versione del sistema; TAIRiX non ha tale
stringa, quindi — diversamente da GNU `sleep` — non ha l'opzione
`--version`.

## OPTIONS

- `-h, -?` — mostrare l'aiuto breve di questo comando.
- `--` — terminare l'analisi delle opzioni; ogni argomento successivo è un
  operando.

## EXAMPLES

- `sleep 5` — mettere in pausa per cinque secondi.
- `sleep 1.5h` — mettere in pausa per novanta minuti.
- `sleep 1m 30s` — mettere in pausa per novanta secondi (gli operandi
  vengono sommati).
- `sleep inf` — mettere in pausa finché il processo non viene terminato.

## EXIT STATUS

- `0` — l'intervallo è trascorso, oppure è stato scritto un aiuto breve
  richiesto.
- `1` — la scrittura dell'aiuto breve è fallita.
- `2` — la riga di comando non è stata compresa (un'opzione sconosciuta, un
  operando mancante o un intervallo di tempo non valido).

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47 come
  `fr-FR`).

## SEE ALSO

- `top`
