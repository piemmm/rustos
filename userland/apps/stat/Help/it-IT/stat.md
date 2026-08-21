## NAME

stat — riportare lo stato di un file o di un filesystem

## SYNOPSIS

`stat [-Lft] [-c FORMATO | --printf=FORMATO] [--] file...`

## DESCRIPTION

Riporta i campi di uno stato letto per operando, nell'ordine della riga di
comando.

**Senza `-L` un collegamento simbolico è descritto come sé stesso**: è a
questo che serve lo strumento accanto a `ls`. `%N` mostra il collegamento
e il bersaglio che memorizza, `%F` dice `symbolic link`, e dimensioni e
marche temporali sono quelle del collegamento. `-L` risolve l'ultimo
collegamento e descrive ciò che nomina.

`-f` passa al filesystem su cui sta l'operando: i conteggi di blocchi e
inode del volume, la sua dimensione di blocco e il tipo che il montaggio
registra. Le due letture hanno vocabolari di campi **diversi**, quindi un
formato è verificato contro quello che `-f` seleziona.

`-c`/`--format` rende una stringa di formato per operando, seguita da un
ritorno a capo; `--printf` interpreta le sequenze di escape e non ne
aggiunge alcuno. È la sola differenza. Una direttiva accetta i flag e la
larghezza di printf (`%-10s`, `%06i`, `%.3n`), così un rapporto può
stare in colonne. `-t` è la forma concisa a una riga di entrambe le
letture.

Un operando illeggibile è segnalato sull'errore standard, gli operandi
restanti sono comunque descritti e il comando termina con stato diverso
da zero. Un campo che questo sistema non può fornire — un'istantanea dei
montaggi che non può leggere, un uid senza nome nella rubrica utenti —
appare come `?` o come `UNKNOWN`, mai come un sostituto plausibile.

Serve almeno un operando. `--` termina l'analisi delle opzioni.

Quattro campi nominano un concetto che TAIRiX non ha e sono
**rifiutati** per nome quando un formato ne usa uno, invece di essere
riempiti con un valore inventato: `%G`, perché la System Information API
pubblica una rubrica utenti e nessuna controparte per i gruppi, quindi
`%g` (l'identificatore numerico) è il campo onesto; `%t` e `%T` del
vocabolario dei file, perché non esistono file speciali di dispositivo
con un tipo maggiore o minore; e `%t` del vocabolario del filesystem,
perché un volume non ha numero magico di tipo — `%T` nomina il tipo che
il montaggio registra. Il rifiuto avviene all'analisi del formato, prima
che sia toccato un percorso.

Due campi riportano un concetto TAIRiX al posto di uno Linux. Un volume è
identificato da un id di 16 byte e non da un numero di dispositivo,
quindi `%d` è quell'id in decimale e `%D` in esadecimale; confrontare il
`%d` di due file risponde ancora esattamente a «stanno sullo stesso
volume?».

## OPTIONS

- `-L, --dereference` — descrivere ciò che un collegamento simbolico
  nomina, anziché il collegamento stesso.
- `-f, --file-system` — descrivere il filesystem che contiene ciascun
  operando anziché l'operando.
- `-c, --format=FORMAT` — rendere `FORMATO` per ciascun operando,
  seguito da un ritorno a capo.
- `--printf=FORMAT` — come `-c`, ma interpretando le sequenze di escape
  e senza ritorno a capo finale.
- `-t, --terse` — stampare i campi su una sola riga separata da spazi.
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `stat note.txt` — il rapporto completo di un file.
- `stat -c '%s %n' *` — dimensione e nome, una riga ciascuno.
- `stat -L collegamento` — descrivere ciò che il collegamento nomina.
- `stat -f .` — il volume che contiene la directory di lavoro.

## EXIT STATUS

- `0` — ogni operando è stato descritto (o è stata scritta la guida
  breve).
- `1` — almeno un operando non è stato leggibile, o l'output è fallito.
- `2` — la riga di comando non è stata compresa, o il suo formato
  nominava una direttiva che questo sistema non può servire.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

ls, readlink, df, du
