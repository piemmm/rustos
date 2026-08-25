## NAME

ping — inviare richieste di eco ICMP a un host di rete

## SYNOPSIS

`ping [option...] host`

## DESCRIPTION

Invia richieste di eco ICMP (IPv4) o ICMPv6 (IPv6) a un host e mostra
ogni risposta con il suo tempo di andata e ritorno, seguito da un
riepilogo finale.

Le richieste passano per un socket di eco ICMP aperto sullo stack di rete
in spazio utente, protetto da `CAP_NET` e `CAP_NET_RAW` e registrato. Lo
stack possiede l'identificatore di eco, così un socket riceve solo le
risposte alle proprie richieste.

La destinazione è un indirizzo IPv4 o IPv6 letterale oppure un nome host.
Un nome viene risolto dal resolver di sistema, usando i server ricorsivi
configurati sulla macchina; un indirizzo letterale non richiede alcuna
interrogazione e funziona quindi anche senza resolver configurato. Un nome
che non risolve ad alcun indirizzo della famiglia richiesta termina
l'esecuzione indicandone la ragione.

Per impostazione predefinita ogni richiesta porta dati casuali ad alta
entropia, estratti di nuovo per ogni richiesta. È deliberato: un
collegamento che comprime o deduplica il traffico riporterebbe altrimenti
una velocità e una latenza che nulla dicono della sua capacità reale. I
byte restituiti sono confrontati con quelli inviati, così un payload
casuale è anche una verifica d'integrità per pacchetto. Con `-p` si sceglie
un motivo fisso quando serve un payload deterministico.

Per impostazione predefinita `ping` invia una richiesta al secondo fino
all'interruzione; `-c` ne limita il numero. Ogni risposta indica origine,
numero di sequenza e tempo; una richiesta senza risposta entro il limite
stampa una riga di scadenza. Il riepilogo finale indica i pacchetti
trasmessi e ricevuti, la percentuale di perdita e i tempi di andata e
ritorno minimo, medio e massimo. `-q` mostra solo l'intestazione e il
riepilogo.

Ogni risposta nomina il pari come `nome (indirizzo)` quando l'indirizzo
ha un record `PTR`, risolto una sola volta per esecuzione con lo stesso
resolver; un indirizzo senza nome, e ogni esecuzione sotto `-n`, stampa
l'indirizzo nudo. `-n` significa inoltre che nessuna query `PTR` viene
messa sulla rete.

Il time-to-live IP non è esposto dall'interfaccia del socket di eco;
diversamente da alcune implementazioni di `ping`, una riga di risposta
non porta quindi un campo `ttl=`.

## OPTIONS

- `-c, --count` — fermarsi dopo questo numero di richieste.
- `-i, --interval` — secondi tra le richieste (un decimale, es. `0.5`).
- `-s, --size` — dimensione del payload in byte.
- `-p, --pattern` — contenuto del payload: `random` (predefinito, alta
  entropia) o una sequenza di cifre esadecimali di lunghezza pari come
  motivo di byte ripetuto, p. es. `-p ff00`.
- `-W, --timeout` — secondi di attesa per ogni risposta.
- `-w, --deadline` — scadenza complessiva dell'esecuzione, in secondi.
- `-4, --ipv4` — richiedere una destinazione IPv4.
- `-6, --ipv6` — richiedere una destinazione IPv6.
- `-n, --numeric` — output numerico: non risolvere il pari all'indietro,
  quindi nessuna query `PTR` viene emessa e le righe di risposta portano
  l'indirizzo nudo.
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
- `2` — riga di comando non compresa, destinazione non risolta, o socket
  non apribile.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `fr-FR`).

## SEE ALSO

- `host`
- `ss`
- `sysinfo`
- `man`
