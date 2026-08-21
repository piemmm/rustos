## NAME

cp — copiare file e directory

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

Copia ogni operando sorgente verso una destinazione. Con una sola
sorgente e una destinazione che non nomina una directory, la sorgente
è copiata su quel percorso esatto. Quando la destinazione nomina una
directory esistente — e sempre quando c'è più di una sorgente — ogni
sorgente è copiata *dentro* quella directory con il proprio nome base.

Una sorgente directory è copiata solo con `-r`, che riproduce
l'intero sottoalbero; senza `-r` un operando directory è rifiutato.
Un file di destinazione esistente è sovrascritto per impostazione
predefinita, saltato con `-n` e richiesto sul flusso di errore
standard con `-i` (una domanda rifiutata salta quella copia senza
errore; una risposta illeggibile non vale mai come consenso).

Il primo fallimento ferma l'esecuzione prima di ogni operando
successivo. `--` termina l'analisi delle opzioni: ogni argomento
successivo è un percorso.

## OPTIONS

- `-r, -R, --recursive` — copiare le directory e il loro contenuto.
- `-f, --force` — quando un file di destinazione non può essere
  creato, rimuoverlo e ritentare la copia una volta.
- `-i, --interactive` — chiedere prima di sovrascrivere un file
  esistente; acconsente solo una risposta che inizia con `y`/`Y`.
- `-n, --no-clobber` — non sovrascrivere mai un file esistente. Vince
  l'ultimo tra `-i` e `-n`.
- `-l, --link` — dare alla destinazione un secondo nome del nodo della
  sorgente invece di copiarne i byte, così i due nomi non possono
  divergere a una scrittura successiva. Una sorgente che è directory
  richiede comunque `-r`.
- `-s, --symbolic-link` — creare un collegamento simbolico che nomina la
  sorgente invece di copiarla.
- `-P, --no-dereference` — riprodurre una sorgente che è collegamento
  simbolico come un collegamento che memorizza lo stesso bersaglio, alla
  lettera, invece di copiare ciò che nomina (così un collegamento
  relativo o pendente sopravvive alla copia). Senza di essa un
  collegamento sorgente è seguito.
- `--preserve=links` — due sorgenti che nominano un solo nodo ottengono
  due nomi alla destinazione invece di due copie, così la copia non
  raddoppia in silenzio lo spazio.
- `-d` — `-P` e `--preserve=links` insieme, come nello strumento GNU.
- `-v, --verbose` — riferire ogni copia come `'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — copiare ogni sorgente dentro
  `dir`, che deve essere una directory esistente. Il valore segue
  attaccato (`-tdir`, `--target-directory=dir`) o come argomento
  successivo.
- `-T, --no-target-directory` — trattare la destinazione come un file
  normale; è permessa esattamente una sorgente. Non combinabile con
  `-t`.
- `-h, -?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `cp notes.txt backup.txt` — copiare un file con un nuovo nome.
- `cp -r Projects Archive` — riprodurre l'albero `Projects` dentro
  `Archive` (o come `Archive` se non esiste).
- `cp -v -t Backup a.txt b.txt` — copiare entrambi i file in
  `Backup`, riferendo ogni copia.

## EXIT STATUS

- `0` — ogni copia è riuscita (un salto per `-n` e una domanda `-i`
  rifiutata non sono fallimenti).
- `1` — un guasto del file system, della domanda o dell'output; il
  motivo è stampato sul flusso di errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `ls`
- `mv`
- `rm`
