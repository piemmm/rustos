## NAME

users — amministrare gli account utente e i gruppi

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

Esegue la sessione interattiva di amministrazione degli account
sull'interfaccia controllata `users_admin`. Ogni operazione è decisa
lato kernel secondo la propria identità attestata dal kernel: senza
`CAP_USER_ADMIN` nel tetto del proprio account ogni operazione è
rifiutata allo smistamento. Le password sono lette con l'eco del
terminale spento e trasformate lato client in un record salato; il
testo in chiaro non attraversa mai l'interfaccia e non è mai mostrato
né registrato.

Lo strumento non accetta operandi: gli account si amministrano con
comandi digitati dentro la sessione.

- `list` — elencare gli account utente.
- `groups` — elencare i gruppi.
- `create <name> <uid> <gid>` — creare un account.
- `passwd <name>` — impostare la password di un account.
- `lock <name>`, `unlock <name>` — disabilitare o riabilitare un
  account.
- `grant <name> <CAP_...>`, `revoke <name> <CAP_...>` — modificare le
  capacità concesse a un account.
- `deluser <name>` — eliminare un account.
- `addgroup`, `delgroup` — creare o eliminare un gruppo.
- `help` — elencare i comandi della sessione.
- `exit`, `quit` — terminare la sessione.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando e uscire.

## EXIT STATUS

- `0` — la sessione è terminata in modo pulito, oppure è stata mostrata
  la guida breve.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `man`
