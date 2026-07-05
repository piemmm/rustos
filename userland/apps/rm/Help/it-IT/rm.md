## NAME

rm — rimuovere file e directory

## SYNOPSIS

`rm [-dfiIrRv] [--] file...`

## DESCRIPTION

Rimuove ogni operando file, in ordine. Un operando che non è una
directory è scollegato; un operando directory è rimosso solo con `-r`
(che rimuove il contenuto in profondità prima e poi la directory
stessa) o, quando è vuota, con `-d`.

Con `-f` un operando inesistente è saltato in silenzio e nessuna
domanda è mai posta. `-i` chiede sul flusso di errore standard prima
di ogni rimozione e prima di scendere in una directory; `-I` chiede
una sola volta prima di rimuovere più di tre operandi o prima di una
rimozione ricorsiva. Una domanda rifiutata salta l'oggetto (o
l'intera esecuzione, con `-I`) senza errore; una risposta illeggibile
non vale mai come consenso. Vince l'ultimo tra `-f`, `-i` e `-I`.

L'operando `/` è rifiutato sotto `--preserve-root`, il comportamento
predefinito. Il primo fallimento ferma l'esecuzione prima di ogni
operando successivo. `--` termina l'analisi delle opzioni: ogni
argomento successivo è un percorso.

## OPTIONS

- `-r, -R, --recursive` — rimuovere le directory e il loro contenuto.
- `-f, --force` — ignorare gli operandi inesistenti; non chiedere
  mai.
- `-d, --dir` — rimuovere le directory vuote.
- `-i, --interactive` — chiedere prima di ogni rimozione; acconsente
  solo una risposta che inizia con `y`/`Y`.
- `-I` — chiedere una sola volta prima di rimuovere più di tre
  operandi, o prima di una rimozione ricorsiva.
- `-v, --verbose` — riferire ogni rimozione come `removed 'file'`.
- `--preserve-root` — rifiutare di rimuovere `/` (il comportamento
  predefinito).
- `--no-preserve-root` — permettere la rimozione di `/`.
- `-h, -?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `rm notes.txt` — rimuovere un file.
- `rm -r Scratch` — rimuovere l'albero `Scratch` e tutto il suo
  contenuto.
- `rm -I a b c d` — chiedere una volta e, con `y`, rimuovere tutti e
  quattro i file.

## EXIT STATUS

- `0` — ogni rimozione è riuscita (una domanda rifiutata e un salto
  per `-f` non sono fallimenti).
- `1` — un guasto del file system, della domanda o dell'output; il
  motivo è stampato sul flusso di errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `cp`
- `ls`
- `mv`
