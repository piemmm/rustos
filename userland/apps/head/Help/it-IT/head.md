## NAME

head — mostrare la prima parte dei file

## SYNOPSIS

`head [option...] [file...]`

## DESCRIPTION

Stampa le prime 10 righe di ogni `file` sullo standard output. Con più
di un `file`, ogni parte è preceduta da un'intestazione
`==> file <==`. Senza `file`, o quando `file` è `-`, viene letto lo
standard input.

`-n` e `-c` cambiano quanto viene stampato: un conteggio semplice
stampa le prime `num` righe o i primi `num` byte; un conteggio scritto
con un `-` iniziale stampa tutto **tranne** le ultime `num` righe o gli
ultimi `num` byte. Un conteggio può portare un suffisso moltiplicatore:
`b` (512), `kB` (1000), `K` (1024), `MB`, `M`, `GB`, `G`, e così via
per `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (una lettera da sola moltiplica
per potenze di 1024; con `B` per potenze di 1000; con `iB` per potenze
di 1024).

La forma storica come primo argomento `head -num` (con i
moltiplicatori `b`/`k`/`m` e le lettere `l`/`q`/`v`/`z` finali
facoltative) è accettata, come nello strumento GNU.

Un file illeggibile viene segnalato sullo standard error e
l'esecuzione continua con il file successivo.

## OPTIONS

- `-c, --bytes <num>` — stampare i primi `num` byte di ogni file; con
  un `-` iniziale, tutto tranne gli ultimi `num` byte.
- `-n, --lines <num>` — stampare le prime `num` righe di ogni file; con
  un `-` iniziale, tutto tranne le ultime `num` righe.
- `-q, --quiet, --silent` — non stampare mai le intestazioni
  `==> file <==`.
- `-v, --verbose` — stampare sempre le intestazioni `==> file <==`.
- `-z, --zero-terminated` — le righe sono delimitate da NUL invece che
  dal carattere di nuova riga.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `head log.txt` — stampare le prime 10 righe di `log.txt`.
- `head -n 3 a b` — stampare le prime 3 righe di `a` e di `b`, ognuna
  sotto la propria intestazione.
- `head -c 1K image` — stampare i primi 1024 byte di `image`.
- `head -n -1 notes` — stampare `notes` senza l'ultima riga.

## EXIT STATUS

- `0` — ogni file è stato stampato (o la guida breve è stata scritta).
- `1` — un file non è stato leggibile, o l'output non è stato
  consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un tag
  BCP-47 come `it-IT`).

## SEE ALSO

- `cat`
- `wc`
- `man`
