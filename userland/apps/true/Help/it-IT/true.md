## NAME

true — non fare nulla, con successo

## SYNOPSIS

`true [argomenti ignorati]`

## DESCRIPTION

Termina con lo stato `0`, ignorando ogni argomento. Gli script lo usano
ovunque serva un comando che riesce sempre: come comando segnaposto,
condizione sempre vera o corpo di un ciclo.

Viene considerato solo un **primo** argomento `-h`, `-?` o `--help` (la
posizione in cui GNU `true` considera `--help`); in qualunque posizione
successiva quelle parole vengono ignorate come tutto il resto.

## OPTIONS

- `-h, -?` — (solo come primo argomento) mostrare la guida breve di
  questo comando.

## EXAMPLES

- `true` — terminare con successo.
- `while true; do …; done` — ripetere fino all'interruzione.

## EXIT STATUS

- `0` — sempre (l'intero scopo dello strumento).
- `1` — non è stato possibile scrivere la guida breve richiesta.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `false`
- `man`
