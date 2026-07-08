## NAME

df — riportare l'uso dello spazio dei filesystem

## SYNOPSIS

`df [option...] [file...]`

## DESCRIPTION

Riporta, una riga per filesystem montato, la dimensione del volume, lo
spazio usato, lo spazio disponibile, la percentuale d'uso e il punto
di montaggio. Con operandi `file` riporta invece il filesystem che
contiene ogni operando (una riga per filesystem, quanti che siano gli
operandi coperti).

Le cifre provengono dall'elenco dei montaggi dell'API di informazioni
di sistema, così come ogni driver di filesystem montato riporta la
propria contabilità. Per impostazione predefinita il rapporto nasconde
i montaggi senza capacità propria (i collegamenti di vista sintetici
del sistema) e i montaggi ulteriori di un volume già elencato; `-a`
mostra tutto, e il numero di voci nascoste è annotato sul flusso di
informazioni standard (fd 3), mai nella tabella.

Le dimensioni sono stampate in blocchi da 1024 byte salvo che
un'opzione di unità scelga diversamente; un'opzione di unità
successiva sostituisce la precedente, e i conteggi di blocchi sono
arrotondati per eccesso. Un filesystem il cui formato alloca gli inode
su richiesta riporta cifre di inode a zero con `-i` — la risposta
onesta «non tracciato».

Un operando `file` che non esiste, o che è un percorso relativo (i
punti di montaggio sono assoluti; `df` non indovina mai una
risoluzione), viene segnalato sull'errore standard e il rapporto
continua con il resto. Le opzioni GNU `--output`, `--sync` e
`--no-sync` non sono ancora disponibili.

## OPTIONS

- `-a, --all` — includere i montaggi senza capacità e duplicati che
  il comportamento predefinito nasconde.
- `-T, --print-type` — aggiungere la colonna del tipo di filesystem.
- `-t, --type <type>` — riportare solo i filesystem del tipo `type`
  (ripetibile).
- `-x, --exclude-type <type>` — omettere i filesystem del tipo
  `type` (ripetibile).
- `-i, --inodes` — riportare i conteggi degli inode invece dell'uso
  dei blocchi.
- `-P, --portability` — il formato portabile POSIX (intestazioni
  `1024-blocks` e `Capacity`).
- `-l, --local` — limitare il rapporto ai filesystem locali (oggi
  ogni montaggio RustOS: non si filtra nulla).
- `--total` — aggiungere una riga etichettata `total` che somma le
  cifre mostrate.
- `-k` — blocchi da 1024 byte (il valore predefinito).
- `-h, --human-readable` — dimensioni leggibili in potenze di 1024
  (`1.0K`, `23M`).
- `-H, --si` — dimensioni leggibili in potenze di 1000 (`1.0k`,
  `23M`).
- `-B, --block-size <size>` — riportare in blocchi di `size` byte
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `df` — l'uso di ogni volume reale in blocchi da 1024 byte.
- `df -h` — lo stesso, in dimensioni leggibili.
- `df /Users/jo` — il filesystem che contiene `/Users/jo`.
- `df -aT` — ogni montaggio, con il suo tipo di filesystem.
- `df --total -k` — i volumi più una riga `total` sommata.

## EXIT STATUS

- `0` — il rapporto ha coperto tutto quanto richiesto (o la guida
  breve è stata scritta).
- `1` — un operando non è stato riportabile, i filtri non hanno
  lasciato nulla, o la richiesta o l'output sono falliti.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta
  BCP-47 come `fr-FR`).

## SEE ALSO

- `du`
- `mount`
- `man`
