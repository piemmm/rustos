## NAME

false — non fare nulla, senza successo

## SYNOPSIS

`false [argomenti ignorati]`

## DESCRIPTION

Termina con lo stato `1`, ignorando ogni argomento. Gli script lo usano
ovunque serva un comando che fallisce sempre: come condizione sempre
falsa o fallimento deliberato.

Viene considerato solo un **primo** argomento `-h`, `-?` o `--help` (la
posizione in cui GNU `false` considera `--help`); in qualunque posizione
successiva quelle parole vengono ignorate come tutto il resto. A
differenza di GNU `false --help`, che termina comunque con `1`, qui una
guida breve servita termina con `0` — la convenzione della guida breve
di RustOS.

## OPTIONS

- `-h, -?` — (solo come primo argomento) mostrare la guida breve di
  questo comando.

## EXAMPLES

- `false` — fallire.
- `until false; do …; done` — eseguire il corpo una volta (la
  condizione è sempre falsa).

## EXIT STATUS

- `1` — sempre (l'intero scopo dello strumento).
- `0` — la guida breve richiesta è stata servita.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `true`
- `man`
