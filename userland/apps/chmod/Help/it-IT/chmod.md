## NAME

chmod — cambiare i bit di modo di un file

## SYNOPSIS

`chmod [-cfRv] [--] MODE file...`

## DESCRIPTION

Cambia i bit di permesso di ogni operando file in `MODE`, in ordine.
`MODE` è un valore ottale assoluto (`644`, `0755`, …) che sostituisce
completamente i bit di permesso, oppure un elenco di clausole
simboliche separate da virgole `[ugoa]*[-+=][rwxXst]*` (`g+w`,
`o-rx`, `a=rx`, `u+s`) che trasformano i bit attuali del file. La `X`
simbolica concede l'esecuzione solo a una directory o a un file che
porti già un bit di esecuzione.

Solo il proprietario di un file può cambiarne il modo; il kernel
rifiuta chiunque altro, e possedere una capability non concede alcuna
deroga. Con `-R` un operando directory viene cambiato e poi il suo
contenuto viene cambiato ricorsivamente. Il primo fallimento ferma
l'esecuzione prima di ogni operando successivo. `--` termina l'analisi
delle opzioni: ogni argomento successivo è un operando. Per un modo
che inizia con `-`, scrivetelo senza il trattino (`a-w`) o terminate
prima le opzioni (`chmod -- -w file`).

## OPTIONS

- `-R, --recursive` — cambiare file e directory ricorsivamente.
- `-c, --changes` — segnalare solo i file il cui modo è davvero
  cambiato.
- `-v, --verbose` — segnalare ogni file elaborato.
- `-f, --silent, --quiet` — sopprimere la maggior parte dei messaggi
  di errore; l'esecuzione fallisce comunque e lo stato di uscita lo
  segnala.
- `-h, -?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `chmod 644 notes.txt` — lettura/scrittura per il proprietario,
  sola lettura per gli altri.
- `chmod g+w shared.txt` — aggiungere la scrittura di gruppo ai bit
  attuali.
- `chmod -R a=rx Docs` — rendere l'albero `Docs` leggibile e
  attraversabile da tutti.

## EXIT STATUS

- `0` — ogni cambio di modo è riuscito.
- `1` — un errore del filesystem o dell'output; la ragione è
  stampata sullo standard error (soppressa sotto `-f`).
- `2` — la riga di comando non è stata compresa, o l'operando di
  modo non era né ottale né simbolico.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47
  come `it-IT`).

## SEE ALSO

- `ls`
- `mkdir`
- `rm`
