## NAME

sysinfo — interrogare le informazioni di sistema

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Invia un'interrogazione tipizzata all'API di informazioni di sistema e
ne mostra la risposta. RustOS non ha `/proc` né `/sys`: questo comando
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
