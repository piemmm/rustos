## NAME

stress — caricare su richiesta CPU, memoria, disco e cache della macchina

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

Avvia processi di lavoro che caricano deliberatamente la macchina,
nello spirito degli strumenti consolidati `stress`/`stress-ng`: cicli
CPU (`--cpu`), lavoratori di memoria alloca-e-tocca (`--vm`),
scrittura/sincronizzazione di piccoli buffer (`--io`), scrittori disco
sequenziali di grandi dimensioni (`--hdd`) e rilettori che agitano le
cache (`--cache`, un'aggiunta TAIRiX). Ogni lavoratore è un proprio
processo paginabile; il processo di controllo fissa la propria memoria
(`mem_pin`, richiede `CAP_MEM_PIN`) per restare reattivo sotto la
pressione che esso stesso crea, e osserva `Ctrl-C`/`Terminate`, così
che ogni fine dell'esecuzione — completamento, timeout o segnale —
ferma i lavoratori, li raccoglie ed elimina ogni file di lavoro.

Gli obiettivi di memoria e disco sono dimensionati sulla macchina
stessa: salvo cifre esplicite con `--vm-bytes`/`--hdd-bytes`, i
lavoratori vm condividono metà della RAM scoperta e gli hdd metà dello
spazio libero del volume di lavoro. `--overcommit P` riscala quegli
obiettivi scoperti al `P` per cento della risorsa; oltre 100 i
lavoratori spingono nella pressione, e i rifiuti tipizzati prodotti
(volume pieno, limite di risorse) sono contati e riportati come esiti
attesi — mai ritentati, mai un crash. Caricare la macchina non richiede
privilegi oltre i limiti di risorse del chiamante — i limiti sono la
difesa, e `stress` li rispetta.

I lavoratori che toccano il disco scrivono solo sotto la directory di
lavoro — la directory cache per utente dell'applicazione
(`$HOME/Library/stress`) salvo che `--temp-path` ne nomini un'altra —
e ogni file di lavoro è rimosso allo smontaggio, inclusi i percorsi
dei segnali.

Alla fine dell'esecuzione viene stampato un riepilogo (soppresso da
`--quiet`), e un record `summary` leggibile da macchina è emesso sul
flusso informativo standard consultivo (fd 3).

## OPTIONS

- `--cpu N`, `--io N`, `--vm N`, `--hdd N` — avviare `N` lavoratori
  del tipo indicato, con il significato di GNU `stress`.
- `--cache N` — avviare `N` agitatori di cache (solo TAIRiX:
  attraversamenti a freddo ripetuti delle directory e riletture
  muovono i registri delle cache recuperabili del kernel).
- `--all N` — `N` lavoratori di ogni tipo.
- `--vm-bytes B`, `--hdd-bytes B` — l'obiettivo in byte di ciascun
  lavoratore, con i suffissi GNU (`k`, `m`, `g`, `t`; ad es. `256M`).
  I valori predefiniti sono dimensionati sulla RAM / sullo spazio
  libero scoperti.
- `--overcommit P` — scalare gli obiettivi vm/hdd scoperti al `P` per
  cento della risorsa; può superare 100 (i rifiuti sono allora esiti
  attesi).
- `--timeout T` — fermarsi dopo `T` (suffissi `s`/`m`/`h`; ad es.
  `5m`). Nessun valore predefinito: senza, l'esecuzione continua
  finché un segnale non la termina.
- `--temp-path DIR` — la directory di lavoro dei lavoratori che
  toccano il disco.
- `--monitor` — eseguire `sysmon` in primo piano per la durata;
  l'esecuzione è riportata quando il monitor termina. Contraddice
  `--background`.
- `-q, --quiet` — sopprimere il riepilogo e le righe di avanzamento
  su stdout (gli errori raggiungono comunque stderr).
- `--background` — stampare il PID del controllore distaccato e
  restituire il prompt (implica `--quiet`). Funziona anche la forma
  `&` della shell; questa opzione è per gli script.
- `-h, -?, --help` — mostrare la guida breve di questo comando e
  uscire.
- `--version` — stampare nome e versione dello strumento e uscire.

## EXIT STATUS

- `0` — l'esecuzione è stata completata (i rifiuti tipizzati dei
  lavoratori sono esiti attesi e non la fanno fallire).
- `1` — un lavoratore è realmente fallito, o l'esecuzione non ha
  potuto essere preparata.
- `2` — la riga di comando non è stata compresa.
- `130` / `143` — `Ctrl-C` / `Terminate` ha terminato l'esecuzione,
  dopo lo smontaggio dei lavoratori e la rimozione dei file di
  lavoro.

## ENVIRONMENT

- `HOME` — individua la directory di lavoro predefinita
  (`$HOME/Library/stress`).
- `LANG` — la lingua preferita della guida breve (un'etichetta BCP-47
  come `it-IT`).

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
