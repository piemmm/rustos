## NAME

sysmon — osservare dal vivo memoria, cache e carico del kernel

## SYNOPSIS

`sysmon [-d sec.decimi] [-h | -?]`

## DESCRIPTION

`sysmon` è una vista dal vivo a schermo intero di ciò che il kernel fa con
la memoria e la CPU, letta interamente tramite l'API di informazioni di
sistema — non c'è alcun `/proc` da raschiare. Mostra la memoria fisica e
la sua composizione, l'heap del kernel, la banda di pressione della
memoria e la sua storia recente, il registro delle cache recuperabili con
i **rapporti di successo** per classe, il livello compresso `ramzip`, il
totale della memoria bloccata, l'uso di archiviazione dei volumi montati,
il carico per CPU, la tabella delle interruzioni del kernel e un censimento
dei processi. Rimane utilizzabile mentre il sistema è sotto carico
deliberato e riposa fra un aggiornamento e l'altro quando è inattivo (la
lettura si parcheggia; non gira mai a vuoto).

All'avvio il monitor blocca la propria memoria (`mem_pin`, che richiede
`CAP_MEM_PIN`) così da non arenarsi mai sui propri page fault sotto la
stessa pressione che osserva. Un blocco rifiutato è segnalato sulla riga
del titolo e la sessione continua sbloccata — il blocco è accessorio, mai
fatale.

Il display si aggiorna a ogni intervallo (3,0 secondi salvo che `-d` lo
cambi). Il monitor non accetta operandi: si guida con i tasti premuti
dentro la sessione.

- `q` — uscire.
- Sinistra / Destra (o `p`) — cambiare il pannello di dettaglio (Sinistra =
  precedente, Destra / `p` = successivo): cache, il livello compresso,
  l'archiviazione dei volumi montati (dischi), il carico per CPU, le linee
  di interruzione, i processi.
- `r` — aggiornare ora.
- `+` / `-` — allungare / accorciare l'intervallo di un secondo, tra 0,1 e
  60 secondi.
- Su/Giù, PagSu/PagGiù, Inizio/Fine — scorrere il pannello a fuoco.
- `h`, `?` — mostrare o nascondere il promemoria dei tasti della sessione
  (che riproduce la legenda delle barre qui sotto).

### Il blocco di riepilogo

Un blocco di riepilogo fisso precede il pannello di dettaglio. Ogni riga è
etichettata a sinistra così da leggersi senza colore; il colore è solo
rinforzo.

- **Riga del titolo** — il nome dello strumento, il tempo di attività del
  sistema (`up D days, H:MM`), le tre medie di carico (1/5/15 minuti) e lo
  stato di blocco (`[pinned]`, oppure `[unpinned: <reason>]` quando il
  blocco è stato rifiutato).
- **`Mem`** — la barra della memoria (vedi la legenda delle barre),
  seguita dai valori usato / totale (unità compatte `K`/`M`/`G`), dalla
  percentuale usata, dalla dimensione dell'heap del kernel e — quando non
  nulli — dai valori dell'archivio compresso `ramzip` e della memoria
  bloccata `pinned`. La barra si restringe per tenere ogni valore su una
  riga di 80 colonne, così non viene mai troncato.
- **`Pres`** — la barra di pressione della memoria: un indicatore a cinque
  bande, ogni banda raggiunta riempita nel proprio colore di gravità,
  seguito dal nome della banda corrente, dai valori libero / riserva e dal
  totale degli ingressi in banda.
- **`Hist`** — la striscia della storia delle bande di pressione: un glifo
  per aggiornamento, il più vecchio a sinistra, ciascuno colorato secondo
  la sua banda — `.` normale, `-` lieve, `=` moderata, `#` grave, `!`
  critica — così che un tratto di pressione si legge come una serie
  colorata.
- **`CPU`** — la barra CPU aggregata (vedi la legenda delle barre), seguita
  dalla percentuale di occupazione di tutte le CPU, dal numero di CPU e dai
  contatori sommati di cambi di contesto e di prelazioni.
- **`Tasks`** — il censimento dei processi: totali, in esecuzione, in
  attesa, fermati e zombie (con `(own)` aggiunto quando il censimento di
  tutti i processi è stato rifiutato e si contano solo le proprie attività).
- **Barra delle schede dei pannelli** — ogni pannello di dettaglio, quello
  a fuoco evidenziato, con un indicatore di scorrimento a destra quando il
  pannello a fuoco trabocca.

### La legenda delle barre

Gli indicatori `Mem` e `CPU` sono barre tra parentesi quadre `[…]`. Il
promemoria `?` riproduce questa legenda dentro la sessione in corso.

La barra della memoria (`Mem`) è una barra **impilata** le cui celle
nominano ciò che la memoria fisica contiene — una ripartizione *disgiunta*
della memoria usata (`used` è `total` meno `free`), così che nulla è
contato due volte e la larghezza riempita è esattamente la frazione usata:

- `#` — memoria residente d'utente (verde): pagine residenti negli spazi
  d'indirizzamento d'utente.
- `K` — l'heap del kernel (ciano): gli heap e le slab propri del kernel.
- `=` — altra memoria in uso (magenta): tutto ciò che è usato ma non
  attribuito sopra (cache di pagine, buffer, frame del kernel).
- vuoto — memoria libera.

L'archivio compresso `ramzip` e la memoria anonima `pinned` si sovrappongono
a quei secchi (le pagine bloccate sono residenti d'utente; l'archivio
compresso è memoria del kernel), perciò sono riportati come valori accanto
alla barra invece che come segmenti separati che conterebbero due volte —
contabilità onesta anziché un'immagine fuorviante.

La barra di pressione (`Pres`) colora ogni banda secondo la sua profondità:
normale/lieve verde, moderata gialla, grave/critica rossa.

La barra CPU (`CPU`) si riempie di celle occupate `#` su una pista inattiva
vuota, colorata secondo la quota occupata (verde sotto il 60 %, giallo sotto
l'85 %, rosso all'85 % o più). TAIRiX contabilizza il tempo di CPU solo come
occupato contro inattivo — non c'è ripartizione utente/sistema/i-o nell'API
— perciò la barra mostra un'unica categoria onesta di occupazione, con il
dettaglio per core nel pannello `cpu`.

### I pannelli di dettaglio

Sinistra / Destra (o `p`) percorre sei pannelli. Ciascuno ha un'intestazione
di colonna invertita (video inverso, grassetto) così che il titolo si legga
come una barra distinta sopra il corpo.

### caches — il registro delle cache recuperabili

Sono le cache che il kernel può restituire per alleviare la pressione della
memoria **senza perdita di dati**: ogni voce è ricostruibile dalla sua fonte
canonica, così il kernel la scarta invece di paginarla. Il pannello è la
risposta diretta a «le cache stanno facendo il loro lavoro?»: ogni riga è
una classe di recupero, aggregata su tutte le cache registrate, e porta il
proprio **rapporto di successo**.

Colonne:

- `class` — la classe di recupero (vedi l'elenco delle classi sotto).
- `entries` — voci vive attualmente trattenute per la classe.
- `cached` — l'impronta residente della classe: il carico utile delle voci
  più i metadati di contabilità per voce, insieme.
- `hits` — ricerche della classe servite dalla cache dall'avvio (la cache
  ha evitato la fonte canonica).
- `misses` — ricerche della classe cadute sulla fonte canonica dall'avvio.
- `hit%` — il rapporto di efficacia della cache, `hits / (hits + misses)`
  come percentuale intera. Un rapporto alto significa che la cache
  ripaga la sua memoria; uno basso, che trattiene memoria senza evitare
  lavoro. Legge `-`, mai uno `0%` inventato, per una classe che nulla ha
  cercato in questo avvio (un denominatore inattivo).
- `ref` — ammissioni **rifiutate** dall'avvio (una voce che la cache ha
  declinato di trattenere: fuori budget, non contabilizzabile, o senza
  memoria).
- `shr` — passate di **riduzione** forzata dalla pressione che hanno
  recuperato voci della classe dall'avvio.
- `fail` — **guasti** interni attribuiti alla classe: un difetto di
  registro rilevato che ha avvelenato (disabilitato fail-closed) una cache.

I conteggi si abbreviano oltre 99 999 come `k`/`M`/`G`/`T` (migliaia
decimali, non KiB) così che una colonna non si allarghi mai.

Le classi di recupero, nell'ordine in cui il kernel le recupera sotto
pressione (la prima elencata è scartata per prima, così una cache in fondo
alla lista sopravvive più a lungo):

- `disposable-ui` — stato d'interfaccia scartabile (risorse rasterizzate,
  atlanti di glifi, istantanee di finestra): il meno costoso da perdere, il
  primo ad andarsene.
- `predictive-prefetch` — dati precaricati in modo speculativo (elenchi,
  miniature, indici di completamento): mai necessari alla correttezza.
- `background-validation` — prodotti di lavoro di validazione a riposo
  (avanzamento di scansione, impronte candidate): il lavoro speculativo si
  ferma appena inizia la pressione.
- `semantic-app-cache` — stato verificato di avvio delle applicazioni
  (manifesti analizzati, riepiloghi di validazione, risultati di
  risoluzione dei comandi). Recuperarlo non può mai rendere un'app non
  avviabile — il varco di caricamento si riesegue e basta.
- `runtime-cache` — stato derivato di proprietà del runtime (preparazione
  del caricatore, mappe di risorse): raggruppato con la cache semantica.
- `clean-file-data` — *contenuto* di file pulito e ricostruibile,
  rileggibile dal volume: una lettura di dispositivo limitata ricostruisce
  un blocco. Recuperato prima che alcunché sia compresso in `ramzip`.
- `transform-cache` — forme intermedie costose di dati autorizzati (dati di
  cluster verificati, decifrati, decompressi): più costose da ricostruire di
  una lettura pulita, perciò recuperate dopo i dati di file puliti.
- `fs-metadata` — metadati del filesystem: record di stato, risultati di
  ricerca di nomi, voci di directory e record di sicurezza. Piccoli, caldi
  e ricostruiti solo da un percorso ad albero in più passi, perciò
  sopravvivono ai dati di file sotto pressione.
- `reliability-assist` — stato ricostruibile di assistenza al recupero
  (finestre di verifica, riepiloghi di salute): giustificato dalla latenza
  di recupero, perciò preservato più a lungo.

### ramzip — il livello di memoria compressa

`ramzip` comprime le pagine anonime fredde in un archivio più piccolo in
RAM invece di paginarle. Le sue sezioni:

- `tier` — l'impronta viva: `entries` trattenute, byte `logical` (non
  compressi) rappresentati, byte `stored` (cifrati) effettivamente
  trattenuti e byte `metadata` di contabilità; poi `saved` (logico meno
  archiviato) con la sua percentuale del logico — la memoria che il livello
  recupera.
- `capacity` — i limiti derivati a cui il livello si dimensiona: `min`
  (sempre disponibile), `soft` (obiettivo), `hard` (soffitto) e i byte
  `pinned` correnti.
- `compress` — la via di archiviazione (scrittura): `attempts` offerti,
  `accepted` e archiviati, e il **tasso di accettazione** (accettati /
  tentativi) — il rapporto di successo proprio di questo livello per la
  compressione. Sotto, la ripartizione dei rifiuti: incomprimibile,
  politica, limite, non idoneo, riserva, quota di attività e rifiuti da
  thrash.
- `restore` — la via di recupero (lettura): `faults` di pagina, ripristini
  `warm`, ripristini `clustered` e il loro totale `restored`; poi i
  `failures` (autenticazione / decodifica) e il **tasso di riuscita**
  (ripristinati / (ripristinati + guasti)). Ogni rapporto è una percentuale,
  o `-` per un denominatore inattivo.
- `warm-up` — gli `attempts` del ripristinatore a caldo in background, il
  suo conteggio `stopped` e il suo conteggio `thrash-detected`.

### disks — archiviazione dei volumi montati

Una riga in stile `df` per volume montato: punto di montaggio, tipo di
filesystem, dimensione totale, usato, disponibile, percentuale d'uso e una
barra d'uso ASCII. Un volume il cui driver non riporta capacità mostra
`capacity unknown` invece di una dimensione inventata; un volume rimosso a
sorpresa o in conflitto di recupero è disegnato nella resa di avviso e
marcato (`[unavailable-dirty]`, `[unavailable-lost]`,
`[recovery-conflict]`). Non ci sono contatori di throughput i-o per
dispositivo nell'API, perciò questi sono capacità e uso onesti, non
velocità di trasferimento inventate.

### cpu — carico per CPU

Una riga per CPU: la sua quota occupata nell'intervallo (`busy%`), la
profondità della sua coda d'esecuzione (`queue`) e i suoi conteggi di cambi
di contesto (`switches`) e di prelazioni (`preemptions`) dall'avvio.

### irqs — linee di interruzione

Una riga per linea di interruzione legata, in ordine crescente di linea:
l'id della linea, l'attività driver proprietaria (`owner`), il `count` di
interruzioni dall'avvio e lo `state` della linea — `active`, oppure
`quarantined` (disegnato nella resa di avviso) quando la rete di sicurezza
del kernel contro le linee impazzite l'ha disabilitata.

### procs — il censimento dei processi

I maggiori consumatori per `%cpu` e per memoria (`size`), ciascuno con il
suo pid, il suo comando e — per la tabella della memoria — il suo stato.
L'elenco interattivo completo dei processi è compito di `top`; questo è
solo il riepilogo del censimento.

### Capacità

Ogni cifra viaggia attraverso l'API di informazioni di sistema. Le query di
statistiche del kernel (memoria, pressione, cache, `ramzip`, carico per CPU)
richiedono `CAP_SYSINFO_KERNEL`; il pannello delle linee di interruzione
richiede `CAP_SYSINFO_HW`; il censimento di tutti i processi richiede
`CAP_SYSINFO_GLOBAL`. Un chiamante privo di una vede il rifiuto di quel
pannello spiegato — mai una cifra inventata — mentre il resto della sessione
continua (fallire chiusi, degradare con grazia). L'archiviazione dei volumi
montati non è soggetta a restrizioni.

## OPTIONS

- `-d, --delay <seconds>` — l'intervallo tra aggiornamenti automatici, in
  secondi con frazione facoltativa (si conserva solo la prima cifra
  decimale, i decimi): `sysmon -d 1.5` aggiorna ogni 1,5 secondi.
  Predefinito 3,0. GNU `top` accetta un intervallo zero e aggiorna il più
  rapidamente possibile; TAIRiX non gira mai a vuoto, perciò uno zero è
  elevato al minimo di 0,1 s.
- `-h, -?` — mostrare la guida breve di questo comando e uscire. Dentro una
  sessione in corso, gli stessi tasti commutano invece il promemoria dei
  tasti.

## EXIT STATUS

- `0` — la sessione è terminata con `q`, oppure è stata mostrata la guida
  breve.
- `1` — il terminale ha fallito; il motivo è scritto sull'uscita d'errore.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un'etichetta BCP-47 come
  `it-IT`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
