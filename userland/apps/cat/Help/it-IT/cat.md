## NAME

cat — concatenare file sullo standard output

## SYNOPSIS

`cat [-AbeEnstTuv] [--] [file...]`

## DESCRIPTION

Legge ogni operando di file in ordine e scrive i suoi byte sullo
standard output. L'operando `-` designa lo standard input, e senza
operandi lo standard input è l'unica sorgente.

Un operando può anche essere un riferimento a risorsa tipizzato come
`sys:random`: viene aperto tramite il risolutore di risorse del sistema
(verificato dalle capability) anziché dal filesystem — `cat sys:random`
emette byte casuali. Un riferimento malformato in uno spazio dei nomi
registrato è un errore, mai un ripiego su un nome di file.

Con `-n` le righe di output sono numerate in modo continuo attraverso
tutte le sorgenti, cosicché una riga a cavallo di due sorgenti è
numerata esattamente una volta, alla comparsa del suo primo byte.
`-b` numera solo le righe non vuote e prevale su `-n`. `-s` sopprime
le righe vuote adiacenti ripetute; una riga soppressa non viene né
scritta né numerata.

Le opzioni di marcatura rendono visibili i byte invisibili: `-E`
stampa `$` prima di ogni fine riga, `-T` stampa TAB come `^I`, e `-v`
stampa gli altri byte di controllo come `^X` e i byte non ASCII in
notazione `M-`. `-e`, `-t` e `-A` sono le consuete combinazioni
`-vE`, `-vT` e `-vET`.

Una sorgente che non può essere letta arresta il comando prima che
qualsiasi sorgente successiva venga toccata; i byte già scritti
restano scritti.

## OPTIONS

- `-A, --show-all` — equivalente a `-vET`.
- `-b, --number-nonblank` — numerare le righe di output non vuote;
  prevale su `-n`.
- `-e` — equivalente a `-vE`.
- `-E, --show-ends` — stampare `$` alla fine di ogni riga.
- `-n, --number` — numerare le righe di output, in modo continuo
  attraverso tutte le sorgenti.
- `-s, --squeeze-blank` — sopprimere le righe vuote adiacenti
  ripetute.
- `-t` — equivalente a `-vT`.
- `-T, --show-tabs` — stampare i caratteri TAB come `^I`.
- `-u` — accettato e ignorato; l'output è già senza buffer.
- `-v, --show-nonprinting` — usare la notazione `^` e `M-` per i byte
  di controllo e non ASCII, tranne il fine riga e TAB.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `cat notes.txt` — scrivere `notes.txt` sullo standard output.
- `cat a.txt - b.txt` — scrivere `a.txt`, poi lo standard input, poi
  `b.txt`.
- `cat -n log.txt` — numerare ogni riga di output.
- `cat -bs draft.txt` — numerare le righe non vuote e comprimere le
  serie di righe vuote.
- `cat -A config.txt` — rendere visibili i fine riga, le tabulazioni
  e i byte di controllo.
- `cat -- -n` — scrivere il file chiamato `-n`.

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
