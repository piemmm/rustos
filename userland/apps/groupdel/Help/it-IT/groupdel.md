## NAME

groupdel — eliminare un gruppo

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

Rimuove un gruppo dal registro dei gruppi. Eliminare un gruppo è un'operazione amministrativa: il registro rifiuta un chiamante privo della capacità di amministrazione degli utenti.

Il registro decide che cosa può essere rimosso. Rifiuta l'eliminazione di un gruppo ancora referenziato da un account, così nessun account nomina un gruppo inesistente.

`--` termina l'analisi delle opzioni: ogni argomento successivo è un operando.

## OPTIONS

- `-h, -?, --help` — mostrare l'aiuto breve proprio di questo comando.

## EXAMPLES

- `groupdel staff` — eliminare il gruppo `staff`.

## EXIT STATUS

- `0` — il gruppo è stato eliminato.
- `1` — il registro ha rifiutato o non ha potuto completare l'eliminazione (per esempio una capacità mancante, un gruppo sconosciuto o un gruppo ancora referenziato); la ragione è stampata sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47 come `it-IT`).

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
