## NAME

mkdir — creare directory

## SYNOPSIS

`mkdir [-pv] [--] directory...`

## DESCRIPTION

Crea ogni directory indicata come operando, in ordine. Senza `-p` la
directory padre di ogni operando deve già esistere e l'operando stesso
non deve esistere; il primo fallimento arresta l'esecuzione prima di
ogni operando successivo.

Con `-p` viene creato prima ogni antenato mancante, dal più esterno al
più interno, e un operando (o antenato) che esiste già come directory
non è un errore. Un antenato che esiste come file fallisce comunque:
nulla viene mai sostituito in silenzio.

L'opzione `-m`/`--mode` di GNU `mkdir` non è ancora accettata: le
directory sono create con il modo predefinito del filesystem finché non
arriverà il meccanismo per impostare i modi; l'opzione arriverà con
esso invece di essere ignorata. `--` termina l'analisi delle opzioni:
ogni argomento successivo è un percorso.

## OPTIONS

- `-p, --parents` — creare le directory padri mancanti; un operando che
  è già una directory non è un errore.
- `-v, --verbose` — segnalare ogni directory creata come
  `mkdir: created directory 'dir'`.
- `-h, -?` — mostrare la guida breve di questo comando (anche
  `--help`).

## EXAMPLES

- `mkdir Notes` — creare una directory nella directory corrente.
- `mkdir -p Projects/os/build` — creare l'intera catena, saltando le
  parti già esistenti.
- `mkdir -pv Home:/tools/bin` — creare sotto una radice alias,
  segnalando ogni nuova directory.

## EXIT STATUS

- `0` — ogni directory è stata creata (o, con `-p`, esisteva già).
- `1` — un errore del filesystem o dell'output; la ragione è stampata
  sullo standard error.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

rmdir, rm, ls
