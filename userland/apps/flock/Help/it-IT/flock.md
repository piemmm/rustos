## NAME

flock — eseguire un comando mantenendo un blocco di file consultivo

## SYNOPSIS

`flock [options] file comando [argomento...]`

## DESCRIPTION

Prende un blocco consultivo che copre l'intero `file`, esegue il `comando`
mentre lo mantiene e termina con lo stato del comando stesso. Due
esecuzioni che nominano lo stesso `file` non si sovrappongono mai, che è
quanto serve a uno script per avere una sola copia di sé in funzione.

Il blocco appartiene al file aperto da questa esecuzione, quindi il sistema
lo rilascia quando l'esecuzione termina: normalmente, interrotta o in
crash. Nel `file` non viene scritto nulla e dopo non c'è nulla da pulire,
così non resta un blocco obsoleto che la prossima esecuzione debba
attendere. Il `file` viene creato se non esiste.

I blocchi sono **consultivi**: coordinano programmi che accettano di
usarli. Non concedono né negano alcun accesso, perciò un programma che non
prende un blocco non è impedito di leggere o scrivere. Chi possa leggere o
scrivere è deciso da proprietario, permessi ed elenco di accesso del file,
esattamente come per ogni altro.

Senza `-n` né `-w` l'esecuzione attende quanto serve. Con `-n` rinuncia
subito; con `-w` dopo il tempo indicato. In entrambi i casi rinunciare
esce con il codice di conflitto (`1` salvo diversa indicazione di `-E`) e
il comando **non** viene eseguito, così uno script distingue sempre «non ho
ottenuto il blocco» da «il comando è fallito».

Tre opzioni dei `flock` di altri sistemi mancano deliberatamente invece di
essere accettate e ignorate. `-u` e `-o` agiscono su un descrittore di file
aperto in precedenza da una shell, forma che questo comando non offre; `-c`
passa il proprio argomento a una shell, che qui si scrive esplicitamente
`flock file elsh -c '...'` perché sia chiaro quale shell venga eseguita.
Chiederne una è un errore d'uso: lo script viene avvisato invece di essere
eseguito senza il blocco che aveva chiesto.

## OPTIONS

- `-s, --shared` — prendere un blocco condiviso. Più esecuzioni possono mantenerlo insieme, e tutte escludono chi ne chieda uno esclusivo. È il blocco dei lettori.
- `-x, --exclusive` — prendere un blocco esclusivo, che esclude ogni altro detentore. Il valore predefinito e il blocco degli scrittori.
- `-n, --nonblock` — non attendere: se il blocco è mantenuto, uscire subito con il codice di conflitto.
- `-w, --timeout <seconds>` — attendere al massimo questo numero di secondi interi, poi uscire con il codice di conflitto.
- `-E, --conflict-code <n>` — lo stato di uscita quando `-n` o `-w` rinuncia. Vale `1` per impostazione predefinita. Scegliete un valore che il comando non restituisca mai se uno script deve distinguerli.
- `-v, --verbose` — riportare sull'errore standard se il blocco è stato preso.
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `flock /Users/ian/Library/backup.lock backup-now` — avviare il
  backup, attendendo se ne è già in corso un'altra copia.
- `flock -n /Storage/db/data.lock compact` — compattare la base di dati, o
  uscire subito con `1` se il blocco è mantenuto.
- `flock -s -w 30 /Storage/db/data.lock report` — prendere un blocco di
  lettore, attendendo fino a trenta secondi la fine di uno scrittore.
- `flock -E 99 -n lock task` — uscire con `99` invece di `1` quando il
  blocco è mantenuto, per distinguerlo da un fallimento di `task`.

## EXIT STATUS

- lo stato del comando — il blocco è stato preso e il comando è stato
  eseguito.
- il codice di conflitto (`1` per impostazione predefinita) — `-n` o `-w`
  ha rinunciato; il comando non è stato eseguito.
- `1` — il blocco non si è potuto prendere per un motivo che attendere non
  risolverebbe, o il comando non si è potuto eseguire; il motivo è stampato
  sull'errore standard.
- `2` — la riga di comando non è stata compresa; nulla è stato bloccato e
  nulla eseguito.
- `126` — il comando è stato trovato ma non si è potuto eseguire.
- `127` — il comando non è stato trovato.

## ENVIRONMENT

- `PATH` — percorso in cui cercare il comando, dopo gli archivi di
  programmi del sistema e dell'utente.
- `HOME` — individua gli archivi di programmi propri dell'utente.
- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

elsh, ps, ulimit
