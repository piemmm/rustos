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
ogni altro byte è rappresentato da un punto. Il contenuto mostrato è
limitato all'inizio del file.

Premere `Invio` per chiedere un altro file. Annullare il selettore
lascia il visualizzatore aperto con un avviso. Chiudere la finestra
dal desktop termina il visualizzatore.

## EXIT STATUS

Zero dopo una chiusura pulita; diverso da zero quando il canale della
finestra o la regione dei fotogrammi condivisa è stata rifiutata (il
motivo è indicato sul flusso di errore standard).
