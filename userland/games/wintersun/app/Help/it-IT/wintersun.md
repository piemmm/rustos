## NAME

wintersun — percorrere un mondo invernale generato proceduralmente

## SYNOPSIS

`wintersun [--reference-scene]`

## DESCRIPTION

Apre una finestra del desktop su un mondo generato: una veduta dall'alto di un
terreno che la macchina sintetizza invece di distribuire, illuminato da un
sole basso che allunga le ombre lungo ogni pendio.

Nulla di questo mondo è memorizzato come immagine. Ogni materiale di cui è
fatto il suolo — neve, roccia, ghiaia, brughiera, tundra — è una manciata di
numeri che il client trasforma in texture mentre disegna, così il mondo appare
identico su ogni macchina e occupa quasi nulla su disco. Le strade si
consumano dentro ciò che attraversano invece di posarvisi sopra.

Il terreno non ancora generato è disegnato come il vuoto che è e si riempie
man mano che arriva. Il client disegna quello che ha invece di fermarsi ad
aspettare, così la finestra continua a rispondere mentre il mondo recupera.

I tasti freccia o `W`, `A`, `S`, `D` fanno camminare. Due tasti tenuti insieme
percorrono la diagonale fra loro alla stessa velocità, e i tasti opposti si
annullano. La veduta vi segue e si ferma al bordo del mondo invece di
scivolarne fuori. I pendii troppo ripidi e l'acqua troppo profonda vi
deviano.

`+` e `-` avvicinano e allontanano la veduta, in cinque passi, da una cella
del mondo larga otto pixel a una larga centoventotto.

`F11` porta la finestra a schermo intero e poi la riporta com'era: una
finestra ingrandita torna ingrandita. `Esc` la ripristina. `Q` esce.

Il client disegna entro un budget per fotogramma. Quando non riesce a
rispettarlo, cede dettaglio in un ordine fisso — densità delle particelle, poi
la risoluzione del buffer di luce, poi il dettaglio dei materiali, poi le
ombre, poi la dimensione di rendering — e la frequenza dei fotogrammi non è
mai ciò che cede. Ogni passo viene restituito quando i fotogrammi sono stati
comodi per un po'. L'ordine è fisso perché ciò che si vede su una macchina
lenta sia prevedibile e non una sorpresa.

Una finestra più grande di quanto il renderer software possa riempire è
disegnata al massimo a 2560×1440 e ingrandita fino alla finestra.

## OPTIONS

- `-h, -?, --help` — mostrare la guida breve di questo comando.
- `--reference-scene` — disegnare la scena di riferimento fissa e tenerla
  ferma: un solo mondo, gli stessi personaggi e lo stesso istante, identici su
  ogni macchina, così che un'immagine della finestra possa essere confrontata
  con una disegnata altrove. `F11` ed `Esc` cambiano ancora le dimensioni
  della finestra; nient'altro si muove.

## EXIT STATUS

`0` quando si esce. Uno stato diverso da zero indica il motivo sull'uscita di
errore: il mondo non ha potuto essere generato, la finestra non ha potuto
essere aperta, oppure il canale degli eventi della sessione è andato perso.

- `2` — la riga di comando non è stata compresa.
- `87` — la scena di riferimento non ha potuto essere disegnata.

## SEE ALSO

`sapper`
