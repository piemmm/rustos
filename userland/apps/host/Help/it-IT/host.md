## NAME

host — risolvere un nome tramite DNS

## SYNOPSIS

`host [-t type] name`

## DESCRIPTION

Risolve un nome di dominio nei suoi indirizzi usando il resolver di base del
sistema e stampa ogni risposta, una per riga. Senza `-t` vengono interrogati
sia i record `A` (IPv4) sia `AAAA` (IPv6); `-t type` limita la ricerca a uno.

I server DNS ricorsivi da interrogare sono letti dalla configurazione
dell'host tramite l'API di informazioni di sistema — lo stesso insieme attivo
riportato dalla lettura `state:net/resolver/servers` — e ogni risposta è
convalidata prima di mostrare un indirizzo. Non esiste `/etc/resolv.conf` né
un file host locale.

Sono supportati solo i record di indirizzo `A` e `AAAA`; gli altri tipi
(`MX`, `TXT` e così via) vengono rifiutati anziché trattati silenziosamente
come `A`. Un nome inesistente stampa `Host <name> not found: 3(NXDOMAIN)`;
quando nessun server è raggiungibile, `host` segnala un timeout sull'uscita
di errore.

## OPTIONS

- `-t, --type` — il tipo di record DNS da interrogare: `A` o `AAAA` (senza
  distinzione di maiuscole). Senza questa opzione vengono interrogati
  entrambi.
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `host example.com` — gli indirizzi IPv4 e IPv6 del nome.
- `host -t AAAA example.com` — solo gli indirizzi IPv6.

## EXIT STATUS

- `0` — è stato trovato almeno un indirizzo (o è stata scritta la guida
  breve).
- `1` — il nome non ha risolto alcun indirizzo (risposta negativa, timeout o
  errore del resolver).
- `2` — la riga di comando non è stata compresa, o l'output non ha potuto
  essere scritto.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un'etichetta BCP-47 come
  `fr-FR`).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
