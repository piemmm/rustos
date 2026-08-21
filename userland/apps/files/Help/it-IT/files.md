## NAME

files — browser grafico del filesystem

## SYNOPSIS

`files [--desktop] [directory] [-h | -?]`

## DESCRIPTION

Apre una finestra del desktop che elenca il filesystem, partendo dalla
`directory` indicata sulla riga di comando, o dalla directory personale
dell'utente che ha avviato il programma quando non ne è indicata
nessuna. La riga in alto mostra il percorso della directory corrente;
le righe sottostanti elencano le voci della directory, con la voce
selezionata evidenziata con il colore d'accento del tema attivo. Ogni
lettura di directory è un normale elenco con controllo dei permessi
sotto l'identità dell'utente che ha avviato il programma: una directory
illeggibile viene rifiutata, mai indovinata.

Il browser si avvia dal pulsante permanente `Files` sulla barra delle
applicazioni o per nome da una shell. Richiede una sessione grafica in
esecuzione: senza di essa il canale finestra è irraggiungibile e il
browser segnala il rifiuto sul flusso di errore standard e termina.

La finestra si comanda con la tastiera: `Giù` e `Su` spostano la
selezione, `Invio` apre la directory selezionata e `Backspace` risale
alla directory superiore. Chiudere la finestra dal desktop termina il
browser.

L'operando `directory` è trattato come input non fidato: deve essere un
percorso assoluto entro il limite di lunghezza dei percorsi del
sistema, e ognuno dei suoi componenti deve essere un vero nome di
directory — `.` e `..` non lo sono, così che una scrittura non possa
mai indicare un luogo diverso da come si legge. Una directory che
infrange una di quelle regole, o che l'utente che ha avviato il
programma non può elencare, viene rifiutata con il motivo sul flusso di
errore standard e la finestra si apre invece sulla directory personale,
così che un argomento sbagliato non lasci mai l'utente senza finestra.
Un secondo operando viene rifiutato del tutto anziché ignorato.

## OPTIONS

- `--desktop` — eseguire come componente gestore di file del desktop stesso:
  una posizione permanente sulla barra delle icone che offre i propri luoghi
  e i volumi montati, nessuna finestra finché non ne viene chiesta una, e
  nessun modo di uscire. La sessione desktop passa questa opzione all'avvio;
  indicare una `directory` accanto ad essa è rifiutato, perché un componente non
  apre alcuna finestra in cui metterla.
- `-h, -?` — mostrare la breve guida di questo comando e uscire.

## EXIT STATUS

Zero dopo una chiusura pulita, o dopo che è stata mostrata la breve
guida; `2` quando la riga di comando non è stata compresa; altrimenti
diverso da zero quando il canale finestra, la regione dei frame
condivisa o l'elenco iniziale della directory è stato rifiutato (il
motivo è indicato sul flusso di errore standard).
