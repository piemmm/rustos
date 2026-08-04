## NAME

wallpaper — selettore grafico dello sfondo del desktop

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Apre una finestra del desktop che offre gli sfondi forniti con il
sistema, il colore dello sfondo dietro di essi e il modo in cui il
desktop organizza le icone sulla sua bacheca. Nulla cambia sullo
schermo finché le impostazioni non vengono applicate.

La griglia elenca ogni sfondo fornito come miniatura, più una voce **No
wallpaper** (Nessuno sfondo) che mostra solo il colore di sfondo scelto.
Ogni miniatura viene renderizzata con l'adattamento attualmente scelto,
in modo che un'anteprima mostri ciò che il desktop farà effettivamente
con quell'immagine. Un file che non può essere decodificato mostra una
tessera segnaposto contrassegnata con il suo nome e non viene tentato di
nuovo.

Le immagini degli sfondi non vengono mai decodificate da questo
programma. Ognuna è renderizzata da un processo sandbox separato che non
detiene alcuna autorità sul filesystem, sulla rete o sull'avvio, quindi
un'immagine malformata non può compromettere il selettore o il desktop.

Le righe delle opzioni sotto la griglia sono:

- **Fit** (Adattamento) — come viene posizionata l'immagine: `fill`
  (copre lo schermo, ritagliando l'eccesso), `fit` (la contiene intera,
  colore di sfondo nelle barre), `stretch` (distorce alla dimensione
  esatta dello schermo), `centre` (dimensione nativa, centrata) e
  `tile` (ripete dall'alto a sinistra).
- **Backdrop** (Sfondo) — il colore piatto mostrato ovunque lo sfondo
  non arrivi: `Theme` segue il tema del desktop attivo, e i colori con
  nome sono fissi. Un colore già in vigore che non è uno di quelli con
  nome viene offerto con la propria dicitura `rrggbb`.
- **Icons** (Icone) — il lato della bacheca da cui cresce la griglia
  delle icone del desktop.
- **Sort** (Ordinamento) — l'ordine in cui sono elencate le icone della
  cartella del desktop.

La finestra è guidata dalla tastiera. `Tab` e `Shift-Tab` spostano il
focus avanti e indietro attraverso la griglia, le righe delle opzioni e
i pulsanti. I tasti freccia consentono di spostarsi all'interno della
griglia delle miniature o di modificare l'opzione focalizzata. `Enter`
attiva il pulsante focalizzato e `Escape` chiude la finestra senza
applicare.

L'applicazione invia le impostazioni scelte alla sessione desktop, che
decide se adottarle, ridisegna la bacheca e le salva per il prossimo
accesso. Questo programma non scrive mai le impostazioni da solo. Il
risultato viene riportato sulla riga di stato sotto le righe delle
opzioni: applicato, rifiutato con il motivo della sessione o nessuna
sessione desktop in ascolto. Un rifiuto lascia la finestra aperta con le
scelte intatte.

Viene offerto solo l'archivio degli sfondi forniti; un'immagine altrove
nel sistema non può essere scelta da questa finestra. I clic del
puntatore non selezionano nulla.

## EXIT STATUS

Zero dopo una chiusura pulita, anche quando le impostazioni sono state
rifiutate. Diverso da zero quando la finestra non può essere aperta, la
regione del frame condiviso è stata rifiutata o il canale della finestra
è andato perduto; il motivo è indicato sullo standard error.

## ENVIRONMENT

`HOME` nomina la directory home dell'utente, sotto la quale viene letto
`Settings/Pinboard/pinboard.conf` all'avvio in modo che la finestra si
apra con le impostazioni in vigore. Tale documento è scritto dalla
sessione desktop, mai da questo programma. Senza `HOME`, la finestra si
apre con i valori predefiniti.

## SEE ALSO

`files`, `viewer`
