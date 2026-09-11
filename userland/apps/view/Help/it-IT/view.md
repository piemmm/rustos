## NAME

view — visualizzatore grafico di immagini e documenti

## SYNOPSIS

`view`

## DESCRIPTION

Apre una finestra del desktop che mostra un'immagine o un documento. Avviato
con un documento — dal gestore dei file, o aprendo un'immagine — mostra quel
file; avviato da solo, chiede al selettore di file fidato della sessione del
desktop di scegliere uno.

Il visualizzatore non detiene alcuna capacità sul filesystem: non può
aprire, elencare né leggere nulla da sé. La sessione naviga a suo nome sotto
la propria identità, e solo il file che l'utente sceglie gli viene delegato,
una sola volta e in sola lettura. Il file non viene mai decodificato nel
visualizzatore stesso: i suoi byte sono trasmessi a un processo di lavoro
separato che non ha alcun accesso al filesystem, così che un file malformato
od ostile non possa raggiungere nulla di ciò che il visualizzatore
raggiunge.

I formati supportati sono JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO e
RISC OS Sprite. Un file che il decodificatore rifiuta dichiara il proprio
motivo nella finestra e sul flusso di errore standard; la finestra non resta
mai vuota e nessuna immagine viene mai inventata.

Più documenti alla volta sono visualizzatori distinti: confrontare due
immagini affiancate significa aprire la seconda. Chiudere una finestra
termina il suo visualizzatore.

La barra degli strumenti in alto porta, nell'ordine: riduci, ingrandisci,
adatta alla finestra, dimensione reale, voce precedente, voce successiva,
ruota a sinistra, ruota a destra, rispecchia, riproduci o metti in pausa
un'animazione, e il pannello informativo. Un cursore di zoom continuo si
trova sul suo bordo finale. La riga di stato in basso dichiara il nome del
documento, il formato, la dimensione in pixel, la voce mostrata, la
lunghezza e l'ingrandimento.

Trascina l'immagine per spostarti al suo interno quando è più grande della
finestra; finché lo è, compaiono barre di scorrimento ai bordi dell'area di
disegno. Gira la rotella sull'immagine per spostarla. Una pressione
secondaria sull'immagine apre il menu del visualizzatore, disegnato dalla
sessione del desktop.

La trasparenza è mostrata su una scacchiera, così che un'immagine
trasparente si legga come trasparente e non come il colore dietro di essa.

* `+` — ingrandisci al passo successivo
* `-` — riduci al passo precedente
* `0` — adatta l'intera immagine alla finestra
* `1` — dimensione reale, un pixel d'immagine per pixel di schermo
* `2` — adatta la larghezza dell'immagine
* `[` / `]` — un quarto di giro a sinistra o a destra
* `M` — rispecchia da sinistra a destra
* `I` — mostra o nascondi il pannello informativo
* `Space` — riproduci o metti in pausa un'animazione
* `O` — scegli un altro documento
* `Page Up` / `Page Down` — voce precedente o successiva
* `Home` / `End` — prima o ultima voce
* tasti freccia — spostarsi nell'immagine
* `Escape` — chiudi la finestra

## OPTIONS

`-h`, `-?`, `--help`
: Scrivere questa guida sull'output standard e terminare.

## EXIT STATUS

Zero dopo una chiusura pulita. Diverso da zero quando il canale della
finestra, la regione di frame condivisa o la sessione del desktop è stata
rifiutata; il motivo è dichiarato sul flusso di errore standard.
