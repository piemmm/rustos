## NAME

sapper — bonificare il campo minato senza colpire una mina

## SYNOPSIS

`sapper`

## DESCRIPTION

Apre una finestra con una griglia di caselle coperte. Alcune nascondono una
mina. Ogni casella scoperta che non ne nasconde una indica quante delle sue otto
vicine la nascondono, e quei numeri bastano a dedurre dove sono le mine. Scopri
ogni casella senza mina e hai vinto; scoprine una con la mina e la partita
finisce.

La prima casella che scopri è sempre sicura, e così le otto attorno: una mossa di
apertura non può quindi mai perdere e apre sempre una zona da cui ragionare.

Fai clic su una casella coperta per scoprirla. Con il pulsante secondario ci
pianti una bandierina, di nuovo un punto interrogativo se attivi, e ancora una
volta la togli. Una casella con la bandierina è protetta: farci clic non fa
nulla.

Una volta aperta una casella, il suo numero può lavorare per te. Fai clic su un
numero le cui bandierine già corrispondono e tutte le vicine restanti vengono
scoperte in un colpo. Facci clic con il pulsante secondario quando le sue vicine
coperte sono esattamente quante il suo numero e vengono tutte segnate in un
colpo. Il pulsante centrale fa come il primo, da qualunque punto della casella.

Il contatore a sinistra mostra quante mine restano da trovare, meno le
bandierine poste; diventa negativo se poni più bandierine di quante siano le
mine. L'orologio a destra parte alla tua prima casella scoperta e si ferma alla
fine della partita. Il pulsante fra i due avvia una nuova partita, e la sua
faccia dice com'è andata quella corrente.

Le frecce spostano un anello sulla griglia. `Space` scopre la casella che vi si
trova, o la accorda se è già aperta. `F` segna la casella, `Shift+F` segna tutte
le sue vicine coperte, `N` avvia una nuova partita, e `1`, `2` e `3` scelgono le
griglie principiante, intermedia ed esperta. Le stesse scelte, e l'impostazione
dei punti interrogativi, sono nel menu della barra delle icone del gioco.

Il tuo tempo migliore su ciascuna delle tre griglie standard è ricordato fra una
sessione e l'altra. Una griglia di dimensioni tue non ne conserva alcuno, perché
due griglie simili non sono mai la stessa partita.

Il gioco si avvia dalla Libreria programmi del desktop, sotto Games, o per nome
da una shell. Richiede una sessione grafica in corso: senza di essa il canale
della finestra è irraggiungibile e il gioco segnala il rifiuto sul flusso di
errore standard e termina.

## EXIT STATUS

Zero dopo una chiusura pulita; diverso da zero quando il canale della finestra o
la regione di frame condivisa è stata rifiutata (il motivo è indicato sul flusso
di errore standard).
