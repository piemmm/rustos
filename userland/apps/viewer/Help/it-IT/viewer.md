## NAME

viewer — visualizzatore grafico di file in sola lettura

## SYNOPSIS

`viewer`

## DESCRIPTION

Apre una finestra del desktop e chiede subito al selettore di file
fidato della sessione desktop di scegliere un file. Il visualizzatore
non possiede alcuna capacità sul filesystem: da solo non può aprire,
elencare né leggere nulla. La sessione naviga per conto del
visualizzatore sotto la propria identità, e solo il file scelto
dall'utente viene delegato al visualizzatore — monouso e in sola
lettura.

Il contenuto del file scelto è mostrato come testo semplice dall'alto
della finestra. I caratteri stampabili sono mostrati così come sono;
ogni altro byte è rappresentato da un punto, cosicché il contenuto
binario appaia ovviamente bonificato. Il contenuto mostrato è
limitato all'inizio del file.

La finestra è guidata dal mouse. Fai clic sul pulsante **Open…** (Apri…)
nell'intestazione per richiedere un altro file. Trascina il cursore
della barra di scorrimento verso l'alto o verso il basso per scorrere
un file lungo, fai clic sulla traccia sopra o sotto il cursore per
cambiare pagina, fai clic sui pulsanti di estremità per avanzare di una
riga o ruota la rotellina sopra la finestra per scorrere. Annullare il
selettore lascia il visualizzatore aperto con un avviso; chiudere la
finestra dal desktop termina il visualizzatore.

La tastiera è una via secondaria per le stesse azioni: `Enter`
richiede un altro file, i tasti freccia avanzano di una riga, Page
Up/Page Down avanzano di una pagina e Home/End saltano all'inizio o
alla fine.

## EXIT STATUS

Zero dopo una chiusura pulita; diverso da zero quando il canale della
finestra o la regione dei fotogrammi condivisa è stata rifiutata (il
motivo è indicato sul flusso di errore standard).
