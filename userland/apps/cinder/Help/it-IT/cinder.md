## NAME

cinder — un compagno da scrivania che vive in un recinto e gira per la scrivania

## SYNOPSIS

`cinder`

## DESCRIPTION

Apre una piccola finestra-recinto con Cinder dentro. Cinder è la mascotte di
TAIRiX: una piccola creatura color ruggine e antracite che gironzola, si
pulisce, sonnecchia e si accorge di dove sia il puntatore.

Lo si può lasciar uscire. Scegliete **Lascia uscire Cinder** dal suo menu nella
barra delle icone e lascerà il recinto per la scrivania stessa, dove passeggia
fra le vostre finestre. Incontrandone una vi si arrampicherà sopra sedendosi
sulla barra del titolo, si appiattirà per sgusciarvi sotto, oppure semplicemente
la aggirerà: quel che fa dipende dalla forma della finestra e dal suo umore.
Avvicinate il puntatore e lo guarderà, lo inseguirà al trotto e gli balzerà
addosso se lo raggiunge.

Fate clic su di lui per accarezzarlo, nel recinto o fuori. Le carezze lo
rallegrano. Fuori solo lui riceve il clic: lo spazio trasparente attorno a lui
appartiene a ciò che sta dietro, così un compagno seduto sopra il vostro lavoro
non ingoia mai un clic destinato a quello.

Nel recinto potete anche prenderlo e posarlo in qualunque punto del pavimento, e
dare colpetti alla palla.

**Il menu.** **Lascia uscire Cinder** / **Riporta Cinder a casa** cambia dove si trova.

**Esci** lo termina e lo toglie dalla scrivania.

**Chiudere il recinto.** Chiudere la finestra del recinto non termina nulla. Cinder è un'applicazione
residente: chiudere il recinto con lui dentro lo mette via, e chiuderlo mentre è
fuori lo lascia vagare. Fate clic sulla sua icona nella barra per riaprire il
recinto. È *Esci* che lo termina.

**Umore.** Cinder vuole tre cose: riposo, gioco e compagnia. Correre consuma la sua energia
e un sonnellino la ripristina; stare fermo lo annoia e inseguire il puntatore lo
diverte; le carezze lo rallegrano. Non dovete gestire nulla di tutto ciò: non
c'è niente da dargli da mangiare e niente va storto se lo lasciate in pace. Serve
perché possiate capire a colpo d'occhio di che umore è.

Come si sente, e se era fuori, viene ricordato fra una sessione e l'altra.

**Quando non può uscire.** Stare sulla scrivania fuori da una finestra richiede la capacità
`CAP_DESKTOP_LAYER`, che questa applicazione chiede nel proprio manifesto
firmato e che anche i permessi del vostro account devono consentire. Se non lo
fanno — o se non c'è una sessione grafica — **Lascia uscire Cinder** ne indica
il motivo nel recinto e sull'errore standard, e il recinto continua a
funzionare. Cinder resta semplicemente dentro.

## EXIT STATUS

`0` per un'uscita pulita. Uno stato diverso da zero è sempre accompagnato dal
motivo sull'errore standard.

## SEE ALSO

`sapper`
