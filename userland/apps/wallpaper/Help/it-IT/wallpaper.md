## NAME

wallpaper — selettore grafico dello sfondo del desktop

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Apre una finestra del desktop che offre gli sfondi forniti con il
sistema, il colore dello sfondo dietro di essi e il modo in cui il
desktop organizza le icone sulla sua bacheca. Nulla cambia sullo
schermo finché le impostazioni non vengono applicate.

La finestra è guidata dal mouse. Una grande anteprima in alto mostra lo
sfondo selezionato così come il desktop lo disegnerà, con il colore
dello sfondo scelto ovunque l'immagine non arrivi. Sotto di essa, la
galleria elenca ogni sfondo fornito come una tessera: clicca su una per
selezionarla e l'anteprima seguirà immediatamente. La tessera **No
wallpaper** (Nessuno sfondo), sempre prima, mostra solo il colore dello
sfondo scelto.

La galleria scorre quando contiene più tessere di quante la finestra ne
mostri. Ruota la rotellina in qualsiasi punto della finestra, trascina
il cursore della barra di scorrimento sul bordo posteriore, o clicca
sulla traccia sopra o sotto il cursore per spostarti di una pagina alla
volta.

Accanto all'anteprima ci sono quattro impostazioni, ciascuna un elenco a
discesa. Clicca su una per aprirla e clicca su una scelta per prenderla:

- **Fit** (Adattamento) — come viene posizionata l'immagine: `fill`
  (copre lo schermo, ritagliando l'eccesso), `fit` (la contiene intera,
  colore di sfondo nelle barre), `stretch` (distorce alla dimensione
  esatta dello schermo), `centre` (dimensione nativa, centrata) e
  `tile` (ripete dall'alto a sinistra).
- **Backdrop** (Sfondo) — il colore piatto mostrato ovunque lo sfondo
  non arrivi: `Theme` segue il tema del desktop attivo, e i colori con
  nome sono fissi. Un colore già in vigore che non è uno di quelli con
  nome viene offerto con la propria dicitura `rrggbb`.
- **Icons** (Icone) — l'angolo della bacheca da cui cresce la griglia
  delle icone del desktop.
- **Sort** (Ordinamento) — l'ordine in cui sono elencate le icone della
  cartella del desktop.

L'anteprima mostra l'immagine, lo sfondo e l'adattamento selezionati
nella propria forma dell'anteprima. Uno schermo di forma diversa
ritaglia o aggiunge barre in modo diverso, quindi l'anteprima è una
visione fedele dell'immagine e della regola di adattamento, non un
modello in scala del display.

Le immagini degli sfondi non vengono mai decodificate da questo
programma. Ognuna è renderizzata da un processo sandbox separato che non
detiene alcuna autorità sul filesystem, sulla rete o sull'avvio, quindi
un'immagine malformata non può compromettere il selettore o il desktop.
Un file che non può essere decodificato è contrassegnato come
`unreadable` nella sua tessera e non viene tentato di nuovo.

La tastiera raggiunge tutto ciò che fa il mouse. `Tab` e `Shift-Tab`
spostano il focus avanti e indietro attraverso la galleria, le quattro
impostazioni e i due pulsanti. I tasti freccia consentono di spostarsi
all'interno della galleria o di aprire l'elenco dell'impostazione
focalizzata e spostarsi al suo interno. `Enter` applica, o attiva il
pulsante focalizzato, e `Escape` chiude la finestra senza applicare.

L'applicazione invia le impostazioni scelte alla sessione desktop, che
decide se adottarle, ridisegna la bacheca e le salva per il prossimo
accesso. Questo programma non scrive mai le impostazioni da solo. Il
risultato viene riportato accanto ai pulsanti: applicato, rifiutato con
il motivo della sessione o nessuna sessione desktop in ascolto. Un
rifiuto lascia la finestra aperta con le scelte intatte.

Viene offerto solo l'archivio degli sfondi forniti; un'immagine altrove
nel sistema non può essere scelta da questa finestra.

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
apra con i valori predefiniti.

## SEE ALSO

`files`, `viewer`
