## NAME

terminal — emulatore di terminale grafico

## SYNOPSIS

`terminal`

## DESCRIPTION

Apre una finestra del desktop che ospita la shell predefinita
dell'utente su uno schermo di 80×25 caratteri. I tasti digitati nella
finestra attiva vengono inviati alla shell; tutto ciò che la shell
scrive (sia l'output standard sia l'errore standard) è interpretato
tramite il vocabolario ANSI/VT condiviso e disegnato con lo schema di
colori scelto nelle impostazioni. Il terminale in sé non fa mai eco:
eco e modifica della riga appartengono alla shell, esattamente come su
una console.

La finestra si apre alle dimensioni misurate dallo schermo 80×25 nella
dimensione del testo in vigore, in modo da adattarsi al display su cui
viene visualizzata; su uno schermo troppo piccolo per quella dimensione,
il testo viene ridotto invece di restringere la finestra, perché un
programma che si imposta per 80 colonne deve comunque ottenerle.

Il terminale si avvia dalla Libreria programmi del desktop (il pulsante
`Library` sulla barra delle applicazioni) o per nome da una shell.
Richiede una sessione grafica in esecuzione: senza di essa il canale
della finestra è irraggiungibile e il terminale segnala il rifiuto sul
flusso di errore standard e termina.

La sessione termina quando la shell esce (per esempio con `exit`) o
quando la finestra viene chiusa dal desktop; chiudere la finestra
termina la shell con fine file sul suo ingresso.

Premendo il tasto secondario (destro) del mouse in qualsiasi punto dello
schermo si apre il menu del terminale. Ogni riga ha una scorciatoia da
tastiera che funziona indipendentemente dal fatto che il menu sia aperto
o meno, e `Escape` — o un clic fuori dal menu — lo chiude senza
effettuare una scelta.

| Riga | Scorciatoia | Cosa fa |
| --- | --- | --- |
| Impostazioni… | `Ctrl ,` | Apre le impostazioni descritte sotto. |
| Testo più grande | `Ctrl +` | Disegna lo schermo un passo più grande. |
| Testo più piccolo | `Ctrl -` | Disegna lo schermo un passo più piccolo. |
| Dimensione reale | `Ctrl 0` | Torna alla dimensione del testo predefinita. |
| Cancella schermo | `Ctrl Shift K` | Pulisce lo schermo senza scrivere sulla shell. |
| Chiudi | `Ctrl Shift W` | Chiude la finestra e termina la shell. |

Le impostazioni si aprono nella finestra stessa e hanno due schede.
**Aspetto** sceglie lo schema di colori, imposta la dimensione del testo
e modifica lo schema dell'utente. Gli schemi forniti sono *System* (che
segue l'aspetto scuro o chiaro del desktop), *Midnight*, *Phosphor*,
*Amber*, *Ember*, *Contrast*, *Paper* e *Custom*. Scegliendo *Custom*
vengono utilizzati i colori modificati sotto il selettore: una griglia
dei venti colori con cui viene disegnato uno schermo — lo sfondo, il
primo piano, il cursore, il testo del cursore e i sedici colori ANSI —
con cursori rosso, verde e blu per quello selezionato.

**Effetti** imposta il modo in cui viene disegnato lo schermo.

| Effetto | Cosa fa |
| --- | --- |
| Opacità | Quanto è solido lo sfondo. Al di sotto del massimo, il desktop traspare dietro il testo, che rimane pienamente leggibile. |
| Sfocatura sfondo | Quanto viene sfocato il desktop dietro una finestra trasparente. Non ha effetto su una finestra completamente opaca. |
| Righe di scansione | Attenua le righe alternate, la parte piatta dell'aspetto di una maschera d'ombra. |
| Alone | Diffonde la luce dei pixel luminosi nei dintorni, in modo che il testo porti l'alone morbido di un tubo spinto al massimo. |
| Disturbo | Un rumore di fondo per pixel in movimento, come quello di un segnale analogico. |
| Fosfori | Quanto a lungo persistono i pixel accesi, in modo che il testo che scorre velocemente lasci una scia. |
| Oscillazione | Una lenta ondulazione orizzontale mobile, come quella di un tubo fuori tempo. |

Ogni modifica ha effetto immediato e viene salvata nel profilo
dell'utente, in modo che un terminale successivo si apra allo stesso modo.
Il sistema operativo custodisce il profilo tramite il proprio servizio
impostazioni, ed è privato del terminale: nessun'altra applicazione può
leggerlo o modificarlo. Viene memorizzato solo ciò che l'utente ha
davvero cambiato, quindi *Ripristina impostazioni predefinite* rimuove
quelle scelte invece di congelare i valori di oggi — si applica allora ciò
che cambia l'amministratore o una versione successiva del terminale. Una
impostazione che il terminale non riesce a interpretare resta al suo valore
predefinito e viene segnalata sul flusso di errore standard, e un servizio
impostazioni non raggiungibile lascia il terminale in funzione con i valori
con cui è distribuito, anch'esso segnalato.

## EXIT STATUS

Zero dopo una chiusura pulita o l'uscita della shell; diverso da zero
quando la shell non ha potuto essere ospitata o quando il canale della
finestra, la regione dei fotogrammi condivisa o la casella degli
eventi è stata rifiutata (la ragione è indicata sul flusso di errore
standard).

## ENVIRONMENT

`HOME`
: La directory home dell'account, dove il terminale legge e scrive il
proprio profilo. Senza di essa il terminale funziona con il profilo
predefinito e non salva nulla.

`TERM`
: Esportata alla shell ospitata come `xterm-256color`, che nomina
l'emulatore presentato da questo terminale. Qualsiasi valore ereditato
viene sostituito; il resto dell'ambiente è inoltrato alla shell
invariato.

## SEE ALSO

`elsh`, `sysinfo`
