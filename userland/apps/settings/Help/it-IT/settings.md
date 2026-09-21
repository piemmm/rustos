## NAME

settings — configurare la scrivania e questa macchina

## SYNOPSIS

`settings`

## DESCRIPTION

Apre una finestra della scrivania che elenca ogni categoria di impostazioni di
questo sistema: che cos'è la macchina, l'aspetto della scrivania, lo schermo,
la rete, i dispositivi con cui viene guidata, gli account che la usano e i
volumi che contiene. Scegliere una categoria nella barra laterale mostra il
suo pannello.

Settings non detiene alcuna autorità propria. Ogni modifica è o una richiesta
alla sessione della scrivania, cui appartengono le impostazioni dell'utente, o
un'esecuzione riautenticata del comando che già scrive quell'archivio: nulla
qui può elevare un privilegio.

Una categoria che questo sistema non può servire lo dice chiaramente e nomina
ciò che dovrebbe esistere perché possa farlo. Un controllo che non cambierebbe
nulla non viene mai mostrato.

Digitare nel campo di ricerca sopra la barra laterale per filtrarla alle
categorie e alle impostazioni che una parola raggiunge. `Tab` e `Shift+Tab`
spostano il fuoco tra campo di ricerca, percorso, barra laterale e pannello;
`Up` e `Down` percorrono la barra laterale e `Enter` apre la riga. Una finestra
troppo stretta abbandona la barra laterale, e la prima briciola del percorso
elenca allora le categorie.

Si avvia dalla riga *Settings…* del menu di sistema della scrivania, dalla
Libreria programmi, o per nome da una shell. Richiede una sessione grafica in
esecuzione: senza di essa il canale della finestra è irraggiungibile e il
programma segnala il rifiuto sul flusso di errore standard e termina.

## EXIT STATUS

Zero dopo una chiusura pulita; diverso da zero quando il canale della finestra
o la regione di fotogrammi condivisa è stata rifiutata (il motivo è indicato
sul flusso di errore standard).
