## NAME

mv — spostare (rinominare) file e directory

## SYNOPSIS

`mv [-finvT] [-t dir] [--] source... dest`

## DESCRIPTION

Sposta ogni operando sorgente verso una destinazione. Con una sola
sorgente e una destinazione che non nomina una directory, la sorgente
è rinominata su quel percorso esatto. Quando la destinazione nomina
una directory esistente — e sempre quando c'è più di una sorgente —
ogni sorgente è spostata *dentro* quella directory con il proprio
nome base.

Uno spostamento all'interno di un volume è una rinomina atomica che
conserva l'identità del nodo. Uno spostamento la cui sorgente e la
cui destinazione stanno su volumi diversi non può essere atomico:
ripiega sulla copia della sorgente verso la destinazione seguita
dalla rimozione della sorgente (le directory sono riprodotte
ricorsivamente).

Una destinazione esistente è sovrascritta per impostazione
predefinita, saltata con `-n` e richiesta sul flusso di errore
standard con `-i` (una domanda rifiutata salta quello spostamento
senza errore; una risposta illeggibile non vale mai come consenso).
Il primo fallimento ferma l'esecuzione prima di ogni operando
successivo. `--` termina l'analisi delle opzioni: ogni argomento
successivo è un percorso.

## OPTIONS

- `-f, --force` — rimuovere una destinazione che blocca e ritentare
  la rinomina; non chiedere mai. Vince l'ultimo tra `-f`, `-i` e
  `-n`.
- `-i, --interactive` — chiedere prima di sovrascrivere una
  destinazione esistente; acconsente solo una risposta che inizia con
  `y`/`Y`.
- `-n, --no-clobber` — non sovrascrivere mai una destinazione
  esistente.
- `-v, --verbose` — riferire ogni spostamento come
  `renamed 'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — spostare ogni sorgente dentro
  `dir`, che deve essere una directory esistente. Il valore segue
  attaccato (`-tdir`, `--target-directory=dir`) o come argomento
  successivo.
- `-T, --no-target-directory` — trattare la destinazione come un file
  normale; è permessa esattamente una sorgente. Non combinabile con
  `-t`.
- `-h, -?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `mv draft.txt final.txt` — rinominare un file.
- `mv -v a.txt b.txt Archive` — spostare entrambi i file in
  `Archive`, riferendo ogni spostamento.
- `mv -n new.cfg current.cfg` — installare un file solo se la
  destinazione non esiste già.

## EXIT STATUS

- `0` — ogni spostamento è riuscito (un salto per `-n` e una domanda
  `-i` rifiutata non sono fallimenti).
- `1` — un guasto del file system, della domanda o dell'output; il
  motivo è stampato sul flusso di errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `cp`
- `ls`
- `rm`
