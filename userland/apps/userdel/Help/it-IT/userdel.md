## NAME

userdel — eliminare un account utente

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

Rimuove un account dal database degli utenti. Eliminare un account è un'operazione amministrativa: il database rifiuta un chiamante privo della capacità di amministrazione degli utenti.

Il database decide che cosa può essere rimosso. Rifiuta l'eliminazione dell'ultimo account attivo che può amministrare gli utenti, così un sistema non resta mai senza modo di essere amministrato.

`--` termina l'analisi delle opzioni: ogni argomento successivo è un operando.

## OPTIONS

- `-h, -?, --help` — mostrare l'aiuto breve proprio di questo comando.

## EXAMPLES

- `userdel ada` — eliminare l'account `ada`.

## EXIT STATUS

- `0` — l'account è stato eliminato.
- `1` — il database ha rifiutato o non ha potuto completare l'eliminazione (per esempio una capacità mancante, un account sconosciuto o l'ultimo amministratore); la ragione è stampata sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47 come `it-IT`).

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
