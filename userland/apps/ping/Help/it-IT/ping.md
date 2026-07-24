## NAME

ping — inviare richieste di eco ICMP a un host di rete

## SYNOPSIS

`ping [option...] indirizzo`

## DESCRIPTION

Invia richieste di eco ICMP (IPv4) o ICMPv6 (IPv6) a un host e mostra
ogni risposta con il suo tempo di andata e ritorno, seguito da un
riepilogo finale.

Le richieste passano per un socket di eco ICMP aperto sullo stack di rete
in spazio utente, protetto da `CAP_NET` e `CAP_NET_RAW` e registrato. Lo
stack possiede l'identificatore di eco, così un socket riceve solo le
risposte alle proprie richieste. In questa versione non c'è risoluzione
dei nomi, quindi la destinazione deve essere un indirizzo IPv4 o IPv6
letterale; un nome host è un errore d'uso, non un fallimento silenzioso.

Per impostazione predefinita `ping` invia una richiesta al secondo fino
all'interruzione; `-c` ne limita il numero. Ogni risposta indica origine,
numero di sequenza e tempo; una richiesta senza risposta entro il limite
stampa una riga di scadenza. Il riepilogo finale indica i pacchetti
trasmessi e ricevuti, la percentuale di perdita e i tempi di andata e
ritorno minimo, medio e massimo. `-q` mostra solo l'intestazione e il
riepilogo.

Il time-to-live IP non è esposto dall'interfaccia del socket di eco;
diversamente da alcune implementazioni di `ping`, una riga di risposta
non porta quindi un campo `ttl=`.

## OPTIONS

- `-c, --count` — fermarsi dopo questo numero di richieste.
- `-i, --interval` — secondi tra le richieste (un decimale, es. `0.5`).
- `-s, --size` — dimensione del payload in byte.
- `-W, --timeout` — secondi di attesa per ogni risposta.
- `-w, --deadline` — scadenza complessiva dell'esecuzione, in secondi.
- `-4, --ipv4` — richiedere una destinazione IPv4.
- `-6, --ipv6` — richiedere una destinazione IPv6.
- `-n, --numeric` — output numerico. Sempre attivo su TAIRiX; accettato
  per familiarità.
- `-q, --quiet` — silenzioso: solo l'intestazione e il riepilogo finale.
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `ping 10.0.2.2` — pingare un host IPv4 fino all'interruzione.
- `ping -c 4 fe80::1` — inviare quattro richieste a un host IPv6.
- `ping -c 10 -i 0.2 10.0.0.1` — dieci richieste, una ogni 200 ms.
- `ping -q -c 100 10.0.0.1` — esecuzione silenziosa, solo riepilogo.

## EXIT STATUS

- `0` — è stata ricevuta almeno una risposta (o è stata scritta la guida).
- `1` — nessuna richiesta ha ricevuto risposta.
- `2` — riga di comando non compresa, o socket non apribile.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `fr-FR`).

## SEE ALSO

- `ss`
- `sysinfo`
- `man`
