## NAME

telnet — il client di terminale virtuale di rete (RFC 854)

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

Apre una connessione TCP verso un host e gli trasmette il terminale: l'output
dell'host compare sullo standard output, i tasti premuti vanno all'host e il
carattere di escape (`^]` per default) apre l'interprete di comandi
`telnet>`. Senza host, `telnet` parte da quel prompt e `open` connette.

È al tempo stesso il modo di raggiungere un servizio a righe su un'altra
macchina e il modo di interrogare a mano qualsiasi servizio TCP:
`telnet host 80` apre una connessione in cui si può digitare una richiesta.

L'host può essere un nome o un indirizzo IPv4/IPv6 letterale. Un nome viene
risolto dal resolver essenziale di sistema, che legge i server DNS ricorsivi
configurati tramite l'API di informazioni di sistema. La porta è un numero:
non esiste un archivio dei servizi, quindi un *nome* di servizio è un errore
d'uso e non un silenzioso ripiego sulla porta 23.

La negoziazione delle opzioni segue la RFC 855 con la disciplina priva di
cicli della RFC 1143, così un pari che si ripete non fa mai ripetere il
client. Le opzioni implementate sono BINARY, ECHO, SUPPRESS GO AHEAD, STATUS,
TIMING MARK, TERMINAL TYPE, NAWS, TERMINAL SPEED, TOGGLE FLOW CONTROL,
LINEMODE e NEW-ENVIRON; ogni altra è rifiutata, che è ciò che significa una
opzione non implementata. LINEMODE (RFC 1184) è implementato per intero — la
maschera `MODE`, la tabella dei caratteri locali (SLC) e `FORWARDMASK` — così
il client modifica la riga come chiede il server, con i caratteri che il
server negozia.

La dimensione della finestra è comunicata via NAWS alla connessione e a ogni
cambiamento. TAIRiX non ha un segnale di ridimensionamento, quindi la
dimensione viene riletta a ogni tasto premuto; un ridimensionamento raggiunge
l'host alla pressione successiva.

`NEW-ENVIRON` rivela **soltanto** le variabili definite ed esportate con il
comando `environ`; il client non invia mai il proprio ambiente. `-a` e `-l`
esportano un nome di accesso, ed è la sola cosa che un'invocazione rivela da
sé.

Due comandi dello strumento storico mancano deliberatamente. Non c'è l'escape
alla shell `!`: a un programma che analizza dati di rete ostili non si
concede l'autorità di avviare una shell. Non c'è `slc check`, perché la
RFC 1184 non gli dà alcuna forma sul cavo distinta da `slc export`.
L'interfaccia socket non espone i dati urgenti TCP, quindi un Synch viaggia
come la sola Data Mark. Quando lo standard input raggiunge la fine del file —
un'invocazione con redirezione come `telnet host 80 < richiesta` — viene
chiuso solo il lato di invio e la sessione continua a leggere finché anche
l'host remoto non chiude, così la risposta non viene scartata come fa lo
strumento storico.

## OPTIONS

- `-4, --ipv4` — connettersi solo tramite IPv4.
- `-6, --ipv6` — connettersi solo tramite IPv6.
- `-8, --binary` — chiedere un percorso dati a 8 bit in entrambe le direzioni.
- `-L, --eight-bit-output` — chiedere un percorso a 8 bit solo in uscita.
- `-E, --no-escape` — nessun carattere di escape; tutto va all'host.
- `-e, --escape <char>` — impostare il carattere di escape (`^]`, `^A`, un solo
  carattere, o vuoto per nessuno).
- `-a, --login` — esportare il nome di accesso della sessione via `NEW-ENVIRON`.
- `-l, --user <name>` — esportare `name` come nome di accesso (implica `-a`).
- `-b, --bind <address>` — associare questo indirizzo locale prima di connettersi.
- `-d, --debug` — tracciare la negoziazione delle opzioni sullo standard error.
- `-?, --help` — mostrare l'aiuto breve di questo comando.

## EXAMPLES

- `telnet example.test` — aprire una sessione sulla porta telnet assegnata.
- `telnet 10.0.2.2 25` — parlare a mano con un servizio di posta.
- `telnet -6 fe80::2` — connettersi solo tramite IPv6.
- `telnet -l ada host` — offrire `ada` come nome di accesso.
- `telnet -8 host` — chiedere un percorso a 8 bit in entrambe le direzioni.
- `telnet` poi `open host` — connettersi dal prompt dei comandi.

## EXIT STATUS

- `0` — la sessione si è svolta (comunque l'host l'abbia terminata), oppure è
  stato scritto l'aiuto breve.
- `1` — la sessione non è stata possibile: l'host non è stato risolto, il
  socket è stato rifiutato, o il terminale non è passato in modo grezzo.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `TERM` — comunicato all'host tramite l'opzione TERMINAL TYPE.
- `USER` — il nome di accesso esportato da `-a`.
- `LANG` — la lingua preferita per l'aiuto breve (un'etichetta BCP-47 come
  `it-IT`).

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
