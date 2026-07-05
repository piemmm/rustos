## NAME

top — osservare la lista dei processi dal vivo

## SYNOPSIS

`top [-h | -?]`

## DESCRIPTION

Mostra una vista a schermo intero, dal vivo, della lista dei processi
tramite l'API di informazioni di sistema, nello spirito del classico
`top`. Parte dai processi del chiamante; la vista dell'intero sistema è
concessa dal servizio solo a un chiamante che detiene
`CAP_SYSINFO_GLOBAL`.

Il visualizzatore non accetta operandi: si controlla con tasti premuti
dentro la sessione.

- `q` — uscire.
- `a` — alternare tra i propri processi e la vista dell'intero
  sistema. Se il servizio rifiuta la vista dell'intero sistema
  (richiede `CAP_SYSINFO_GLOBAL`), il visualizzatore resta sui propri
  processi e la riga di stato ne indica il motivo; la sessione
  continua.
- `r` — aggiornare la lista.
- Su/Giù, PagSu/PagGiù, Inizio/Fine — spostare la selezione.
- `h`, `?` — mostrare o nascondere il riepilogo dei tasti.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando e uscire. In una
  sessione in corso, gli stessi tasti alternano invece il riepilogo dei
  tasti.

## EXIT STATUS

- `0` — la sessione è terminata con `q`, oppure è stata mostrata la
  guida breve.
- `1` — il servizio o il terminale è fallito; il motivo è stampato
  sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
