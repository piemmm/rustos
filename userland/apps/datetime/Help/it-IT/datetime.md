## NAME

datetime — impostare la data e l'ora della macchina

## SYNOPSIS

`datetime`

## DESCRIPTION

Apre una finestra del desktop che mostra l'orologio della macchina in sei
campi modificabili — anno, mese e giorno sulla prima riga, ora, minuto e
secondo sulla seconda — e imposta l'orologio su quanto indicano. Nulla
cambia finché non si premi **Set**.

La lettura è in UTC. TAIRiX non mantiene alcuno scostamento di fuso
orario, quindi non c'è un'ora locale da mostrare né da inserire.

Alla finestra si arriva normalmente dal menu dell'orologio del desktop:
fare clic sull'orologio nella barra delle icone e scegliere **Set Date &
Time…**. Impostare l'orologio richiede un'autorità che una sessione
desktop non possiede, quindi il desktop chiede un account che la possieda
e questa applicazione viene avviata con quell'account una volta accettata
la password.

Fare clic su un campo per scrivervi, oppure premere `Tab` per passare al
successivo. Sono accettate solo cifre, con un `-` iniziale consentito
nell'anno per una data precedente all'anno 1. `Enter` imposta
l'orologio; `Escape` chiude la finestra.

Ogni campo è controllato prima che qualcosa venga impostato, e il primo
errore è dichiarato nella finestra invece di essere corretto in silenzio:
un mese fuori da 1 a 12, un'ora fuori da 0 a 23, un minuto o un secondo
fuori da 0 a 59, o un giorno che non esiste nel mese e nell'anno inseriti
— il 31 aprile, o il 29 febbraio fuori da un anno bisestile. Quando un
campo è rifiutato non viene impostato nulla.

Le date precedenti al 1970 e molto successive al 2038 sono voci
ordinarie. L'orologio è un valore a 64 bit con segno: nessuna delle due è
un limite.

Se l'orologio della macchina non è mai stato impostato da quando è
partita, i campi si aprono **vuoti** e la finestra lo dichiara. Non
vengono riempiti con l'epoca Unix, che sarebbe una data che la macchina
non ha mai affermato.

Se l'account con cui questa applicazione è in esecuzione non può
impostare l'orologio, il tentativo è rifiutato, la finestra lo dichiara e
l'orologio resta esattamente com'era. Il motivo è scritto anche sul
flusso di errore standard. L'applicazione continua a funzionare: un
rifiuto è una risposta, non un guasto del programma.

## EXIT STATUS

Zero dopo una chiusura pulita, anche quando un'impostazione è stata
rifiutata. Diverso da zero quando la finestra non ha potuto essere
aperta, la regione di frame condivisa è stata rifiutata o il canale della
finestra è stato perso; il motivo è dichiarato sul flusso di errore
standard.

## SEE ALSO

`sysinfo`, `uptime`
