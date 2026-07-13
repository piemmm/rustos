## NAME

sysmon — osservare in diretta memoria e carico del kernel

## SYNOPSIS

`sysmon [-d sec.decimi] [-h | -?]`

## DESCRIPTION

Mostra a schermo intero, in diretta, memoria e carico del kernel
tramite l'API di informazioni di sistema: memoria fisica, heap del
kernel, banda di pressione della memoria con il suo storico, registro
delle cache recuperabili, livello compresso `ramzip`, totale della
memoria bloccata, carico per CPU e censimento dei processi. Lo
strumento resta usabile sotto carico deliberato e riposa tra un
aggiornamento e l'altro quando il sistema è inattivo.

All'avvio il monitor blocca la propria memoria (`mem_pin`, che richiede
`CAP_MEM_PIN`) per non fermarsi mai sui propri page fault sotto la
stessa pressione che osserva. Un blocco rifiutato viene riportato sulla
riga del titolo e la sessione continua senza blocco — il blocco è
accessorio, mai fatale.

Lo schermo si aggiorna a ogni intervallo (3,0 secondi salvo `-d`), e
`r` lo aggiorna subito. Il monitor non accetta operandi: si controlla
con i tasti dentro la sessione.

- `q` — uscire.
- `p` — scorrere il pannello di dettaglio: cache recuperabili, livello
  compresso, carico per CPU, processi.
- `r` — aggiornare subito.
- `+` / `-` — allungare / accorciare l'intervallo di un secondo, tra
  0,1 e 60 secondi.
- Su/Giù, PagSu/PagGiù, Inizio/Fine — scorrere il pannello.
- `h`, `?` — mostrare o nascondere il riepilogo dei tasti.

Sei righe di riepilogo precedono il pannello di dettaglio: il titolo
(tempo di attività, medie di carico e stato del blocco); le cifre di
memoria in MiB con il totale bloccato; la banda di pressione con il suo
indicatore, le cifre libero/riserva e i contatori d'ingresso; lo
storico delle bande (un glifo per aggiornamento: `.` normale, `-`
lieve, `=` moderata, `#` severa, `!` critica); la riga CPU complessiva;
e il censimento dei task.

Ogni cifra passa per l'API di informazioni di sistema — non esiste
`/proc`. Le interrogazioni statistiche del kernel richiedono
`CAP_SYSINFO_KERNEL`, e il censimento di tutti i processi
`CAP_SYSINFO_GLOBAL`: a chi ne è privo viene spiegato il rifiuto di
quel pannello mentre il resto della sessione continua. L'elenco
interattivo completo dei processi è compito di `top`; il pannello
processi mostra qui solo il censimento e i maggiori consumatori per
`%CPU` e per memoria.

## OPTIONS

- `-d, --delay <seconds>` — l'intervallo tra gli aggiornamenti
  automatici, in secondi con frazione facoltativa (si conserva solo la
  prima cifra decimale, i decimi): `sysmon -d 1.5` aggiorna ogni 1,5
  secondi. Predefinito 3,0. GNU `top` accetta un intervallo zero e
  aggiorna il più in fretta possibile; RustOS non gira mai a vuoto,
  quindi lo zero viene alzato al minimo di 0,1 s.
- `-h, -?` — mostrare la guida breve di questo comando e uscire.
  Dentro una sessione in corso, gli stessi tasti attivano invece il
  riepilogo dei tasti.

## EXIT STATUS

- `0` — la sessione è terminata con `q`, o è stata mostrata la guida
  breve.
- `1` — il terminale ha fallito; il motivo è scritto sull'errore
  standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `it-IT`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
