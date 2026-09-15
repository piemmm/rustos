## NAME

sapper — das Minenfeld räumen, ohne eine Mine auszulösen

## SYNOPSIS

`sapper`

## DESCRIPTION

Öffnet ein Fenster mit einem Raster verdeckter Felder. Unter einigen liegt eine
Mine. Jedes aufgedeckte Feld ohne Mine zeigt, wie viele seiner acht Nachbarn
eine tragen, und diese Zahlen genügen, um die Minen zu erschließen. Decken Sie
jedes minenfreie Feld auf und Sie haben gewonnen; decken Sie eine Mine auf, ist
die Partie vorbei.

Das erste aufgedeckte Feld ist immer sicher, ebenso die acht ringsum. Ein
Eröffnungszug kann daher nie verlieren und öffnet stets einen Bereich, aus dem
sich schließen lässt.

Klicken Sie ein verdecktes Feld an, um es aufzudecken. Mit der zweiten Taste
setzen Sie eine Fahne, noch einmal ein Fragezeichen, sofern eingeschaltet, und
noch einmal löschen Sie die Markierung. Ein mit Fahne markiertes Feld ist
geschützt: Ein Klick darauf bewirkt nichts.

Ist ein Feld offen, kann seine Zahl die Arbeit übernehmen. Klicken Sie auf eine
Zahl, deren Fahnen bereits passen, und alle übrigen Nachbarn werden auf einmal
aufgedeckt. Klicken Sie mit der zweiten Taste darauf, wenn genau so viele
Nachbarn verdeckt sind wie die Zahl angibt, und alle werden auf einmal markiert.
Die mittlere Taste tut dasselbe wie die erste, von jeder Stelle des Feldes aus.

Der Zähler links zeigt, wie viele Minen noch zu finden sind, abzüglich der
gesetzten Fahnen; er wird negativ, wenn Sie mehr Fahnen setzen, als es Minen
gibt. Die Uhr rechts startet mit Ihrem ersten aufgedeckten Feld und hält am
Partieende an. Die Schaltfläche dazwischen beginnt eine neue Partie, und ihr
Gesicht sagt, wie die laufende ausging.

Die Pfeiltasten bewegen einen Ring über das Raster. `Space` deckt das Feld darin
auf oder akkordiert es, wenn es bereits offen ist. `F` markiert das Feld,
`Shift+F` markiert alle verdeckten Nachbarn, `N` beginnt eine neue Partie, und
`1`, `2` und `3` wählen das Anfänger-, Fortgeschrittenen- und Expertenraster.
Dieselben Auswahlen und die Fragezeichen-Einstellung stehen im Symbolleistenmenü
des Spiels.

Ihre Bestzeit auf jedem der drei Standardraster bleibt zwischen Sitzungen
erhalten. Ein Raster eigener Größe behält keine, denn zwei solche Raster sind nie
dieselbe Partie.

Das Spiel wird aus der Programmbibliothek des Desktops unter Games gestartet oder
namentlich aus einer Shell. Es setzt eine laufende grafische Sitzung voraus: ohne
sie ist der Fensterkanal nicht erreichbar, und das Spiel meldet die Ablehnung auf
dem Standardfehlerstrom und endet.

## EXIT STATUS

Null nach sauberem Schließen; ungleich null, wenn der Fensterkanal oder die
gemeinsame Rahmenregion abgelehnt wurde (der Grund steht auf dem
Standardfehlerstrom).
