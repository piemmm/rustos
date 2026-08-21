## NAME

readlink — stampare il bersaglio di un collegamento simbolico

## SYNOPSIS

`readlink [-fem] [-nz] [-q | -s | -v] [--] file...`

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

`-f`, `-e` e `-m` passano invece alla **canonizzazione**: l'unico
percorso che nomina ciò a cui l'operando si risolve, con ogni
collegamento seguito e ogni `..` applicato. Con nessuna di esse
l'operando deve essere un collegamento, e le tre differiscono solo per
quanta parte del percorso deve esistere. Sono alternative e non
modificatori, quindi vince l'ultima data.

Quella risoluzione è del filesystem — `..` fisico, il budget di salti,
un controllo del permesso di ricerca su ogni directory attraversata e la
regola che un collegamento non può risolversi fuori da ciò che il suo
montaggio proietta — e questo strumento la *chiama* invece di seguire i
collegamenti da sé. Una seconda copia dell'algoritmo che divergesse per
una regola stamperebbe un percorso che il filesystem risolve
diversamente.

## OPTIONS

- `-f, --canonicalize` — stampare il percorso canonico; ogni componente
  tranne l'ultimo deve esistere.
- `-e, --canonicalize-existing` — stampare il percorso canonico; ogni
  componente deve esistere.
- `-m, --canonicalize-missing` — stampare il percorso canonico; nessun
  componente deve esistere.
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
- `readlink -f alias` — stampare ciò a cui si risolve, collegamenti
  compresi.
- `readlink -z a b | tr '\0' '\n'` — bersagli separati da NUL per uno
  script.

## EXIT STATUS

- `0` — il bersaglio di ogni operando è stato stampato (o è stata scritta
  la guida breve).
- `1` — almeno una lettura è stata rifiutata, o l'output è fallito.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
