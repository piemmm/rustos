## NAME

whoami — stampare il nome dell'account dell'utente corrente

## SYNOPSIS

`whoami`

## DESCRIPTION

Stampa il nome utente associato all'identità di questo processo,
seguito da un ritorno a capo, e nient'altro.

TAIRiX non ha `/etc/passwd`: l'identificatore utente proviene dal
registro che il kernel tiene del processo chiamante, e il nome
dell'account corrispondente proviene dall'elenco pubblico degli
account dell'API di informazioni di sistema. Se l'elenco non contiene
alcun nome per l'identificatore, il comando segnala
`cannot find name for user ID <uid>` e fallisce.

Il comando non accetta operandi; un argomento è un errore
`extra operand`.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando.
- `--` — terminare l'analisi delle opzioni; ogni argomento successivo
  resta un operando di troppo (`whoami` non ne accetta alcuno).

## EXAMPLES

- `whoami` — stampare il nome dell'account che esegue il comando.

## EXIT STATUS

- `0` — il nome (o la guida breve richiesta) è stato scritto.
- `1` — la lettura dell'identità, la consultazione dell'elenco o
  l'output è fallita, oppure l'elenco non contiene alcun nome per
  l'identificatore utente.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un tag
  BCP-47 come `fr-FR`).

## SEE ALSO

- `users`
- `ps`
