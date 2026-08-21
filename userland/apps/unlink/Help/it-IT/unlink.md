## NAME

unlink — rimuovere un solo nome

## SYNOPSIS

`unlink [--] file`

## DESCRIPTION

Rimuove esattamente un nome, tramite l'unica chiamata al filesystem che
la funzione POSIX `unlink` nomina. Non ci sono deliberatamente né
ricorsione, né forzatura, né conferma, né resoconti: uno script che deve
rimuovere un solo nome e nient'altro ha uno strumento che non può fare di
più. Per quelle opzioni c'è `rm`, per una directory `rmdir`.

Il nome è rimosso **così come è scritto**. Un collegamento simbolico è
rimosso esso stesso e non è mai seguito, così un collegamento piazzato lì
non può dirottare la rimozione sul suo bersaglio.

Una **directory** è rifiutata dal filesystem, nello stesso
attraversamento bloccato che avrebbe rimosso la voce: qui non esiste
alcuna corsa fra il controllo e la rimozione.

Serve esattamente un operando: nessun operando e due o più operandi sono
entrambi errori d'uso, e nulla è rimosso. `--` termina l'analisi delle
opzioni, così un nome che inizia con un trattino resta rimovibile.

## OPTIONS

- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `unlink vecchio.log` — rimuovere un nome.
- `unlink Home:/Documents/alias` — rimuovere il collegamento simbolico
  stesso, non ciò che indica.
- `unlink -- -nome-strano` — rimuovere un nome che inizia con un
  trattino.

## EXIT STATUS

- `0` — il nome è stato rimosso (o è stata scritta la guida breve).
- `1` — il filesystem ha rifiutato la rimozione, o l'output è fallito;
  la ragione è stampata sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

rm, rmdir, ln, link, readlink
