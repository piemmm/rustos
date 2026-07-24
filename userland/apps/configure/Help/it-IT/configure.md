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
  servizio di accesso avvia per un utente autenticato. `text` (il
  predefinito) avvia la shell dell'account — il desktop può comunque
  essere avviato su richiesta con il comando `desktop`; `graphical`
  avvia direttamente la sessione desktop dopo l'autenticazione quando
  un desktop è installato, ripiegando sul testo quando non lo è.
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
