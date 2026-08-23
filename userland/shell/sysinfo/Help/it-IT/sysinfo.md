## NAME

sysinfo — interrogare le informazioni di sistema

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Invia un'interrogazione tipizzata all'API di informazioni di sistema e
ne mostra la risposta. TAIRiX non ha `/proc` né `/sys`: questo comando
è il volto da terminale della stessa API versionata e controllata da
capacità che ogni programma usa, e nessun percorso elude il controllo
di capacità.

Le interrogazioni:

- `processes`, `ps` — elencare i processi, una riga per processo.
- `memory`, `mem` — statistiche di memoria del kernel (richiede
  `CAP_SYSINFO_KERNEL`).
- `hardware`, `hw` — l'albero hardware rilevato (richiede
  `CAP_SYSINFO_HW`).
- `identity`, `id` — identità della macchina e versione del SO.
- `uptime` — tempo dall'avvio e l'ora di avvio.
- `limits`, `rlimits` — i propri limiti di risorse effettivi e il loro
  uso in tempo reale.
- `seats` — l'inventario dei posti: il proprietario di ogni display e
  la sua console in primo piano (richiede `CAP_SYSINFO_HW`).
- `pressure` — l'indicatore di pressione della memoria in tempo reale:
  banda, soglie e contatori di transizione (richiede
  `CAP_SYSINFO_KERNEL`).
- `reclaim` — il registro delle cache recuperabili, una riga per classe
  (richiede `CAP_SYSINFO_KERNEL`).
- `ramzip` — i contatori del livello di memoria compressa (richiede
  `CAP_SYSINFO_KERNEL`).
- `cpu` — profondità della coda, cambi di contesto e prelazioni per CPU
  (richiede `CAP_SYSINFO_KERNEL`).
- `irq`, `irqs` — la tabella IRQ del kernel: una riga per ogni linea di
  interrupt associata — il suo id, il task del driver proprietario, il
  numero di interrupt dall'avvio e se la linea è in quarantena (richiede
  `CAP_SYSINFO_HW`).
- `cpuinfo` — il rapporto sul processore per CPU (un soprainsieme di
  `/proc/cpuinfo`): modello/produttore, classe di prestazioni, flag delle
  estensioni ISA, il registro di identità grezzo, la frequenza di clock
  del core misurata dal vivo (in MHz — o un onesto «unknown» dove non
  esiste alcun contatore di clock del core) e la frequenza fissa di
  riferimento o base tempi. Dati pubblici sull'hardware, nessuna
  capacità richiesta.
- `storage`, `io` — la salute dell'I/O di archiviazione per volume: una
  riga per ogni volume a blocchi consapevole dei guasti — un prefisso del
  suo identificatore durevole, l'endpoint del servizio a blocchi che lo
  serve, la sua disponibilità corrente
  (available/degraded/recovering/lost) e i contatori cumulativi degli
  esiti (completamenti, reset, timeout, errori del supporto, riemissioni)
  su cui un disco guasto o instabile diventa visibile (richiede
  `CAP_SYSINFO_KERNEL`).
- `raid`, `arrays` — gli array RAID composti e i dispositivi che il
  compositore di array detiene: una riga per array — un prefisso della
  sua identità, il suo livello, la sua salute
  (optimal/degraded/recovering/failed), il numero di membri sincronizzati
  e definiti, la sua unità di striping, il suo numero di blocchi e
  qualsiasi ricostruzione o verifica in corso — poi una riga per
  dispositivo — il suo nodo dell'albero hardware, l'array a cui
  appartiene (un trattino per un candidato non affiliato), il suo slot,
  il suo ruolo (candidate/held/in-sync/resyncing/faulted), la sua
  dimensione e la generazione di metadati che porta (richiede
  `CAP_SYSINFO_HW`).
- `show <resource-ref>` — legge un riferimento a risorsa
  `info:`/`state:`/`stats:` e stampa il suo valore. Quegli spazi dei nomi
  servono valori tipizzati tramite questa API, mai flussi di byte: `cat` non
  può aprirli. Un rifiuto nomina la capability necessaria.
- `describe <resource-ref>` — stampa la busta della risposta invece del
  valore: il produttore, l'autorizzazione con cui è stata servita e i
  metadati del payload — per una metrica il genere, l'unità, il
  comportamento di azzeramento e la finestra di campionamento; per un fatto
  il tipo e la riservatezza.
- `help` — la guida breve di questo comando.

Senza interrogazione viene mostrata la guida breve.

## OPTIONS

- `--all, -a` — con `processes`: elencare tutti i processi del sistema
  anziché solo i propri; il servizio concede questa vista solo a un
  chiamante che detiene `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `sysinfo identity` — stampare l'identità della macchina e la versione
  del SO.
- `sysinfo ps --all` — elencare tutti i processi del sistema.

## EXIT STATUS

- `0` — l'interrogazione è stata risposta e mostrata.
- `1` — il servizio ha rifiutato o è fallito, oppure il risultato non è
  stato consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `man`
- `ps`
- `top`
