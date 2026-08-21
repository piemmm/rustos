## NAME

readlink — stampare il bersaglio di un collegamento simbolico

## SYNOPSIS

`readlink [-nz] [-q | -s | -v] [--] file...`

## DESCRIPTION

Stampa il bersaglio che ciascun operando memorizza, uno per operando,
nell'ordine della riga di comando.

Il bersaglio è stampato **così come è memorizzato**. Il bersaglio di un
collegamento è un dato, non un percorso risolto quando il collegamento fu
creato: può essere relativo, contenere `..` e non nominare nulla. Così
`readlink` mostra la scrittura, e `ls -l` mostra un collegamento accanto a
ciò che nomina adesso.

Un operando che **non** è un collegamento simbolico non ha bersaglio da
stampare — un file e una directory sono entrambi rifiutati con la stessa
ragione «valore fuori intervallo» — e un nome assente è «non trovato». In
entrambi i casi gli operandi restanti sono comunque letti e il comando
termina con stato diverso da zero. Il silenzio è il valore predefinito,
come nello strumento GNU: `-v` accende le diagnosi per operando.

`-n` omette il delimitatore dopo l'ultimo bersaglio. Con più di un
operando è ignorato, e ciò è segnalato, perché i delimitatori fra i
bersagli sono ciò che li separa.

Serve almeno un operando. `--` termina l'analisi delle opzioni.

Le opzioni di canonizzazione GNU `-f`, `-e` e `-m` sono **rifiutate**, non
approssimate. Risolvere ogni componente di un percorso — seguire ogni
collegamento, trattare `..` fisicamente, applicare il budget di salti e la
regola che un collegamento non può uscire dal volume che lo memorizza — è
l'unica implementazione del filesystem. Una seconda copia qui potrebbe
stampare un percorso che il filesystem risolve diversamente, quindi
l'opzione fallisce finché il filesystem non offre quella risoluzione da sé.

## OPTIONS

- `-n, --no-newline` — non stampare il delimitatore dopo l'ultimo
  bersaglio (ignorato, con segnalazione, per più di un operando).
- `-z, --zero` — terminare ogni bersaglio con NUL invece che con
  ritorno a capo.
- `-q, -s` — non diagnosticare una lettura rifiutata (il predefinito;
  anche `--quiet`, `--silent`).
- `-v, --verbose` — diagnosticare una lettura rifiutata sull'errore
  standard.
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `readlink Home:/Desktop/Notes` — stampare ciò che una scorciatoia
  memorizza.
- `readlink -v alias` — stamparlo, e dire perché se non è un
  collegamento.
- `readlink -z a b | tr '\0' '\n'` — bersagli separati da NUL per uno
  script.

## EXIT STATUS

- `0` — il bersaglio di ogni operando è stato stampato (o è stata scritta
  la guida breve).
- `1` — almeno una lettura è stata rifiutata, o l'output è fallito.
- `2` — la riga di comando non è stata compresa, o nominava un'opzione di
  canonizzazione.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
