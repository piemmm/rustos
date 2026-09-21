## NAME

settings — den Desktop und diesen Rechner einrichten

## SYNOPSIS

`settings`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das jede Einstellungskategorie dieses Systems
auflistet: was der Rechner ist, wie der Desktop aussieht, der Bildschirm, das
Netzwerk, die Eingabegeräte, die Konten, die ihn benutzen, und die Datenträger,
die er enthält. Die Auswahl einer Kategorie in der Seitenleiste zeigt deren
Bereich.

Settings besitzt keine eigene Befugnis. Jede Änderung ist entweder eine Anfrage
an die Desktop-Sitzung, welcher die Einstellungen des Benutzers gehören, oder
ein erneut authentifizierter Aufruf des Befehls, der diesen Speicher bereits
schreibt; nichts hier kann Rechte ausweiten.

Eine Kategorie, die dieses System nicht bedienen kann, sagt das deutlich und
nennt, was dafür vorhanden sein müsste. Ein Bedienelement, das nichts ändern
würde, wird niemals gezeigt.

Tippen Sie in das Suchfeld über der Seitenleiste, um sie auf die Kategorien und
Einstellungen zu filtern, die ein Wort erreicht. `Tab` und `Shift+Tab` wechseln
zwischen Suchfeld, Pfadleiste, Seitenleiste und Bereich; `Up` und `Down` laufen
durch die Seitenleiste und `Enter` öffnet die Zeile. Ein zu schmales Fenster
lässt die Seitenleiste weg, und der erste Eintrag der Pfadleiste listet dann
die Kategorien auf.

Gestartet wird es über die Zeile *Settings…* im Systemmenü des Desktops, über
die Programmbibliothek oder namentlich aus einer Shell. Es benötigt eine
laufende grafische Sitzung: ohne sie ist der Fensterkanal unerreichbar, und es
meldet die Ablehnung auf dem Standardfehlerstrom und beendet sich.

## EXIT STATUS

Null nach einem saubereren Schließen; ungleich null, wenn der Fensterkanal oder
der gemeinsame Rahmenbereich abgelehnt wurde (der Grund steht auf dem
Standardfehlerstrom).
