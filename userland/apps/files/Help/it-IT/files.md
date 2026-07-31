## NAME

files — browser grafico del filesystem

## SYNOPSIS

`files`

## DESCRIPTION

Apre una finestra del desktop che elenca il filesystem, partendo dalla
vista radice. La riga in alto mostra il percorso della directory
corrente; le righe sottostanti elencano le voci della directory, con la
voce selezionata evidenziata con il colore d'accento del tema attivo.
Ogni lettura di directory è un normale elenco con controllo dei
permessi sotto l'identità dell'utente che ha avviato il programma: una
directory illeggibile viene rifiutata, mai indovinata.

Il browser si avvia dal pulsante permanente `Files` sulla barra delle
applicazioni o per nome da una shell. Richiede una sessione grafica in
esecuzione: senza di essa il canale finestra è irraggiungibile e il
browser segnala il rifiuto sul flusso di errore standard e termina.

La finestra si comanda con la tastiera: `Giù` e `Su` spostano la
selezione, `Invio` apre la directory selezionata e `Backspace` risale
alla directory superiore. Chiudere la finestra dal desktop termina il
browser.

## EXIT STATUS

Zero dopo una chiusura pulita; diverso da zero quando il canale
finestra, la regione dei frame condivisa o l'elenco iniziale della
directory è stato rifiutato (il motivo è indicato sul flusso di errore
standard).
