## NAME

configure — leggere e impostare la configurazione di sistema all'avvio

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Elenca, mostra e imposta le voci dell'archivio di configurazione in
`/System/Settings/Configuration/system.conf`. Senza operandi ogni voce
è elencata con il valore attuale; con una sola chiave ne è mostrato il
valore; con chiave e valore la voce viene modificata.

L'archivio risiede sul volume radice cifrato e i suoi consumatori lo
leggono dopo lo sblocco del file system radice; una modifica ha quindi
effetto al successivo avvio del suo consumatore (`os.loginType`:
l'accesso del prossimo avvio; gli interruttori `cache.*`: lo sblocco
del prossimo avvio).

L'insieme delle chiavi è chiuso: una chiave sconosciuta, o un valore
fuori dall'insieme di una chiave, è rifiutato indicando le scelte
valide e non cambia nulla. Modificare una voce riscrive l'archivio in
forma canonica e richiede l'accesso in scrittura a `/System/Settings`:
un account ordinario può leggere le impostazioni ma non cambiarle.

- `os.loginType` — `text` o `graphical`: quale tipo di sessione il
  servizio di accesso avvia per un utente autenticato. `graphical` (il
  predefinito) avvia direttamente la sessione desktop dopo
  l'autenticazione, ripiegando sull'accesso testuale su una macchina che
  non può eseguirne una; `text` avvia la shell dell'account — il desktop
  può comunque essere avviato su richiesta con il comando `desktop`.
- `cache.all` — `on` o `off`: l'interruttore principale della cache.
  `on` (il predefinito) lascia che ogni classe di cache sottostante
  segua la propria impostazione; `off` è un tetto che disabilita ogni
  cache in memoria a prescindere dalle impostazioni per classe.
- `cache.filesystem`, `cache.block`, `cache.transform`,
  `cache.semantic` — `auto` o `off`: gli interruttori per classe per le
  quattro cache di memoria recuperabili (le cache del file system, del
  blocco dell'intero disco, del cluster decompresso e dell'avvio delle
  applicazioni). `auto` (il predefinito) lascia che il gestore della
  pressione di memoria governi la classe; `off` la disabilita del
  tutto. Non esiste un `on` per classe: una classe non può essere
  forzata a ignorare la pressione di memoria. Una classe è di fatto
  `off` ogni volta che `cache.all` è `off`.

Ogni cache è un acceleratore recuperabile, mai la fonte di verità,
quindi spegnerne una o tutte rende soltanto più lento il lavoro
interessato — non cambia mai un risultato.

- `net.ipv4.enabled`, `net.ipv6.enabled` — `true` o `false`: gli
  interruttori delle famiglie di indirizzi a livello di stack. Entrambi
  sono `true` per impostazione predefinita. Una famiglia disattivata non
  assegna indirizzi, non risponde ad alcun pacchetto e rifiuta un socket
  di quella famiglia con un errore tipizzato — mai uno scarto
  silenzioso.
- `net.ipv6.privacy` — `true` o `false`: se lo stack forma indirizzi
  IPv6 temporanei (di privacy) oltre a quello stabile. `false` (il
  valore predefinito) usa solo l'indirizzo SLAAC stabile.
- `net.tcp.syncookies` — `auto` o `always`: la difesa dalle inondazioni
  SYN. `auto` (il valore predefinito) mantiene una coda semiaperta
  limitata e ripiega su cookie senza stato in caso di overflow;
  `always` risponde a ogni richiesta di connessione senza stato. Non
  esiste `off` — una coda di connessioni indifesa non è
  un'impostazione.
- `net.tcp.keepalive` — `true` o `false`: se le connessioni TCP inviano
  sonde keepalive su un collegamento inattivo. `false` (il valore
  predefinito) non sonda mai e non chiude mai una connessione inattiva;
  `true` sonda un pari inattivo dopo l'intervallo consueto e chiude la
  connessione se smette di rispondere.
- `net.tcp.ecn` — `true` o `false`: se le connessioni TCP negoziano la
  notifica esplicita di congestione (ECN). `false` (il valore
  predefinito) lascia le connessioni Not-ECT; `true` offre ECN
  nell'handshake e poi tratta un contrassegno di congestione come un
  segnale di rallentamento invece di forzare la perdita di un pacchetto.
- `time.servers` — `none` oppure un elenco di server di ora di rete
  separati da virgole, ciascuno un nome host o un indirizzo. `none` (il
  valore predefinito) significa che l'orologio non viene mai impostato
  dalla rete: TAIRiX non ha un proprio insieme di server di ora, quindi
  indicare un server è una scelta dell'operatore.
- `time.refresh` — `6h`, `12h`, `1d`, `2d` o `7d`: quanto tempo di
  attività passa fra due interrogazioni dell'orologio una volta nota
  l'ora. `1d` è il valore predefinito. Un orologio non impostato, non
  plausibile o molto vecchio viene corretto appena la rete lo consente,
  qualunque cosa dica questa impostazione.

Lo stack di rete legge le impostazioni `net.*`; una modifica ha effetto
quando lo stack applica di nuovo la sua configurazione.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `configure` — elencare tutte le voci.
- `configure os.loginType` — mostrare il tipo di sessione predefinito.
- `configure os.loginType graphical` — avviare nell'accesso grafico.
- `configure cache.all off` — disabilitare ogni cache in memoria in
  tutto il sistema.
- `configure cache.filesystem off` — disabilitare solo la cache del
  file system.

## EXIT STATUS

- `0` — elenco, valore, guida breve o modifica completati.
- `1` — l'archivio non si è potuto leggere o scrivere (per esempio il
  chiamante non può cambiare le impostazioni di sistema), oppure
  l'output non si è potuto consegnare.
- `2` — la riga di comando non è stata compresa, la chiave è
  sconosciuta o il valore è fuori dall'insieme della chiave.

## ENVIRONMENT

- `LANG` — la lingua preferita della guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

- `man`
