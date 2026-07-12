## NAME

terminal — emulatore di terminale grafico

## SYNOPSIS

`terminal`

## DESCRIPTION

Apre una finestra del desktop che ospita la shell predefinita
dell'utente su uno schermo di 80×24 caratteri. I tasti digitati nella
finestra attiva vengono inviati alla shell; tutto ciò che la shell
scrive (sia l'output standard sia l'errore standard) è interpretato
tramite il vocabolario ANSI/VT condiviso e disegnato con la tavolozza
del tema attivo. Il terminale in sé non fa mai eco: eco e modifica
della riga appartengono alla shell, esattamente come su una console.

Il terminale si avvia dal menu start del desktop (la voce `Terminal`)
o per nome da una shell. Richiede una sessione grafica in esecuzione:
senza di essa il canale della finestra è irraggiungibile e il
terminale segnala il rifiuto sul flusso di errore standard e termina.

La sessione termina quando la shell esce (per esempio con `exit`) o
quando la finestra viene chiusa dal desktop; chiudere la finestra
termina la shell con fine file sul suo ingresso.

## EXIT STATUS

Zero dopo una chiusura pulita o l'uscita della shell; diverso da zero
quando la shell non ha potuto essere ospitata o quando il canale della
finestra, la regione dei fotogrammi condivisa o la casella degli
eventi è stata rifiutata (la ragione è indicata sul flusso di errore
standard).
