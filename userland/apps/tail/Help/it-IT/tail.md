## NAME

tail — mostrare l'ultima parte dei file

## SYNOPSIS

`tail [option...] [file...]`

## DESCRIPTION

Stampa le ultime 10 righe di ogni `file` sullo standard output. Con più
di un `file`, ogni parte è preceduta da un'intestazione `==> file <==`.
Senza `file`, o quando `file` è `-`, viene letto lo standard input.

`-n` e `-c` cambiano quanto viene stampato: un conteggio semplice (o
scritto con un `-` iniziale) stampa le ultime `num` righe o byte; un
conteggio scritto con un `+` iniziale stampa tutto **a partire** dalla
riga o dal byte `num` (contando da 1) fino alla fine. Un conteggio può
portare un suffisso moltiplicatore: `b` (512), `kB` (1000), `K` (1024),
`MB`, `M`, `GB`, `G`, e così via per `T`, `P`, `E`, `Z`, `Y`, `R`, `Q`
(una lettera sola moltiplica per potenze di 1024; con `B` per potenze di
1000; con `iB` per potenze di 1024).

La forma storica come primo argomento `tail -num` / `tail +num` (con una
lettera finale `b`/`c`/`l` facoltativa) è accettata, come nello strumento
GNU.

La modalità di inseguimento tiene ogni file aperto e stampa i nuovi dati man mano che cresce; si blocca finché il file non cambia — mai attesa attiva. `-f` segue il descrittore; `-F` segue il nome e riapre un file ruotato, e `--retry` attende che un nome compaia. `--pid=PID` termina l'inseguimento quando il processo finisce (verificato ogni `--sleep-interval` secondi, predefinito 1; `--max-unchanged-stats` predefinito 5). Un troncamento viene segnalato e il file inseguito dal suo nuovo inizio.

Quando del contenuto iniziale non viene mostrato, un record informativo
è scritto sul flusso di informazioni standard (fd 3); non cambia mai
l'output né lo stato di uscita. Un file non leggibile è segnalato sullo
standard error e l'esecuzione continua con il file successivo.

## OPTIONS

- `-c, --bytes <num>` — stampare gli ultimi `num` byte di ogni file; con
  un `+` iniziale, tutto dal byte `num` in poi.
- `-n, --lines <num>` — stampare le ultime `num` righe di ogni file; con
  un `+` iniziale, tutto dalla riga `num` in poi.
- `-q, --quiet, --silent` — non stampare mai le intestazioni
  `==> file <==`.
- `-v, --verbose` — stampare sempre le intestazioni `==> file <==`.
- `-z, --zero-terminated` — le righe sono delimitate da NUL invece che
  dall'a capo.
- `-f, --follow[=descriptor]` — seguire per descrittore, stampando i dati aggiunti.
- `-F` — seguire per nome (`--follow=name --retry`); riaprire un file ruotato.
- `--follow=name` — seguire il nome anziché il descrittore.
- `--retry` — continuare a provare ad aprire un file non ancora presente.
- `--pid <PID>` — fermare l'inseguimento quando il processo `PID` muore.
- `--sleep-interval <N>` — secondi tra i controlli (predefinito 1).
- `--max-unchanged-stats <N>` — cicli fermi prima che `-F` ricontrolli il nome (predefinito 5).
- `-h, -?` — mostrare l'aiuto breve di questo comando.

## EXAMPLES

- `tail log.txt` — stampare le ultime 10 righe di `log.txt`.
- `tail -n 3 a b` — stampare le ultime 3 righe di `a` e di `b`, ciascuna
  sotto la sua intestazione.
- `tail -c 1K image` — stampare gli ultimi 1024 byte di `image`.
- `tail -n +5 notes` — stampare `notes` dalla sua 5ª riga alla fine.

## EXIT STATUS

- `0` — ogni file è stato stampato (o l'aiuto breve è stato scritto).
- `1` — un file non ha potuto essere letto, o l'output non ha potuto
  essere consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

- `head`
- `cat`
- `wc`
- `man`
