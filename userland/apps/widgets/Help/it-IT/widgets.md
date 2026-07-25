## NAME

widgets — galleria di componenti Reactive Alloy

## SYNOPSIS

`widgets`

## DESCRIPTION

Apre una finestra del desktop che mostra ogni controllo grafico condiviso di
TAIRiX in una propria scheda: pulsanti, selettori, controlli di valore, campi
di testo, controlli di scelta, raccolte, barre, superfici di riscontro e
controlli della finestra. Ogni scheda presenta diverse varianti della propria
famiglia — ruoli, stati e valori differenti — così che il comportamento
completo di ciascun controllo sia visibile e interattivo in un unico posto.

Cambia scheda facendo clic sulla barra delle schede oppure con i tasti `Left`,
`Right`, `Home` ed `End` ed `Enter`. Fai clic su un controllo per interagire
con esso: un interruttore commuta, un cursore si sposta, un campo di testo
riceve il cursore, una casella combinata si apre. Un controllo su cui si è
fatto clic mantiene il focus della tastiera, così le frecce, `Enter`, `Space` e
i caratteri digitati lo comandano; `Tab` e `Shift+Tab` spostano il focus tra la
barra delle schede e i controlli.

La galleria si avvia dal menu di avvio del desktop o per nome da una shell.
Richiede una sessione grafica in corso: senza di essa il canale della finestra
è irraggiungibile e la galleria segnala il rifiuto sul flusso di errore
standard e termina.

## EXIT STATUS

Zero dopo una chiusura pulita; diverso da zero quando il canale della finestra
o la regione di fotogrammi condivisa è stata rifiutata (il motivo è indicato
sul flusso di errore standard).
