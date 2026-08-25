## NAME

ss — elencare i socket aperti

## SYNOPSIS

`ss [option...]`

## DESCRIPTION

Elenca i socket aperti del sistema, una riga per socket: il protocollo
di trasporto, lo stato della connessione, il riempimento delle code di
ricezione e di invio, la `address:port` locale e remota e — con `-p` —
il processo proprietario.

Le righe provengono dall'elenco dei socket dell'API di Informazioni di
Sistema, che lo stack di rete risponde come interrogazione privilegiata
e verificata: nomina i socket di ogni principale e il pari di ogni
connessione, perciò elencare tutti i socket richiede
`CAP_SYSINFO_GLOBAL`. Non esiste `/proc/net`; a una sessione priva di
quella capacità viene comunicato e `ss` termina, invece di stampare una
tabella vuota.

Per impostazione predefinita l'elenco mostra i socket connessi, non in
ascolto. `-l` mostra solo i socket in ascolto e `-a` entrambi; il numero
di ascoltatori nascosti è annotato sul flusso di informazioni standard
(fd 3), mai nella tabella. `-t` e `-u` restringono il protocollo e
`-4`/`-6` la famiglia di indirizzi; senza alcuno, si mostrano tutti i
protocolli e le famiglie. Le porte sono sempre numeriche (TAIRiX non ha
un database dei nomi di servizio), quindi `-n` è accettato ma per esse
sempre in vigore. Anche gli indirizzi sono numerici, a meno che `-r` non
chieda i nomi host: `-r` risolve ciascuno con il resolver di sistema
(una query `PTR`), interroga una sola volta ogni indirizzo distinto e
lascia numerico quello senza nome. Un indirizzo non specificato si
stampa come `*` e una porta non legata come `*`; un indirizzo IPv6 è tra
parentesi quadre affinché il separatore `:port` resti privo di ambiguità
— un nome risolto non ne ha bisogno.

`ss` accetta solo opzioni. La grammatica delle espressioni di filtro di
iproute2 (filtri di stato e di indirizzo) non è implementata, quindi un
operando nudo è un errore d'uso e non un argomento ignorato in silenzio.

## OPTIONS

- `-t, --tcp` — mostrare i socket TCP. Senza `-t` né `-u`, si mostrano
  entrambi i protocolli.
- `-u, --udp` — mostrare i socket UDP.
- `-a, --all` — mostrare i socket in ascolto e connessi.
- `-l, --listening` — mostrare solo i socket in ascolto.
- `-n, --numeric` — non risolvere i nomi di servizio. Sempre in vigore
  su TAIRiX; accettato per familiarità. I nomi host spettano a `-r`.
- `-r, --resolve` — risolvere gli indirizzi in nomi host via DNS.
  Disattivo per impostazione predefinita: l'elenco non interroga se non
  richiesto.
- `-p, --processes` — aggiungere la colonna del processo proprietario
  (`pid=N`).
- `-4, --ipv4` — restringere l'elenco ai socket IPv4.
- `-6, --ipv6` — restringere l'elenco ai socket IPv6.
- `-H, --no-header` — sopprimere la riga di intestazione.
- `-s, --summary` — stampare i totali di difesa delle connessioni TCP
  dello stack invece della tabella dei socket.
- `-?, --help` — mostrare l'aiuto breve di questo comando.

## EXAMPLES

- `ss` — i socket connessi, non in ascolto.
- `ss -a` — ogni socket, in ascolto e connesso.
- `ss -l` — solo i socket in ascolto.
- `ss -tlp` — i socket TCP in ascolto, con il processo proprietario.
- `ss -u4` — i socket UDP su IPv4.
- `ss -r` — lo stesso elenco con gli indirizzi risolti in nomi host.

## EXIT STATUS

- `0` — l'elenco è stato prodotto (o l'aiuto breve è stato scritto).
- `1` — l'interrogazione dei socket è stata rifiutata o è fallita, o
  l'output non è stato scritto.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

- `ping`
- `sysinfo`
- `man`
