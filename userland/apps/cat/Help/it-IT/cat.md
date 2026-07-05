## NAME

cat — concatenare file sullo standard output

## SYNOPSIS

`cat [-n] [--] [file...]`

## DESCRIPTION

Legge ogni operando di file in ordine e ne scrive i byte sullo
standard output. L'operando `-` designa lo standard input, e senza
operandi lo standard input è l'unica sorgente.

Con `-n` le righe di output sono numerate in modo continuo attraverso
tutte le sorgenti, così una riga divisa fra due sorgenti è numerata
esattamente una volta, alla comparsa del suo primo byte.

Una sorgente che non può essere letta arresta il comando prima che una
sorgente successiva sia toccata; i byte già scritti restano scritti.

## OPTIONS

- `-n, --number` — numerare le righe di output, in modo continuo
  attraverso tutte le sorgenti.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `cat notes.txt` — scrivere `notes.txt` sullo standard output.
- `cat a.txt - b.txt` — scrivere `a.txt`, poi lo standard input, poi
  `b.txt`.
- `cat -n log.txt` — numerare ogni riga di output.
- `cat -- -n` — scrivere il file di nome `-n`.

## EXIT STATUS

- `0` — ogni sorgente è stata scritta.
- `1` — una sorgente non è stata letta, o l'output non è stato
  consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `ls`
- `man`
