## NAME

files — grafischer Dateisystem-Browser

## SYNOPSIS

`files`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das das Dateisystem auflistet, beginnend
mit der Wurzelansicht. Die oberste Zeile zeigt den Pfad des aktuellen
Verzeichnisses; die Zeilen darunter listen die Einträge des
Verzeichnisses, der ausgewählte Eintrag ist mit der Akzentfarbe des
aktiven Themas hervorgehoben. Jedes Lesen eines Verzeichnisses ist eine
gewöhnliche, berechtigungsgeprüfte Auflistung unter der Identität des
startenden Benutzers: ein nicht lesbares Verzeichnis wird abgelehnt,
niemals erraten.

Der Browser wird über die permanente Schaltfläche `Files` in der
Taskleiste oder namentlich aus einer Shell gestartet. Er benötigt eine
laufende grafische Sitzung: ohne sie ist der Fensterkanal unerreichbar,
und der Browser meldet die Ablehnung auf dem Standardfehlerstrom und
beendet sich.

Das Fenster wird mit der Tastatur bedient: `Runter` und `Hoch` bewegen
die Auswahl, `Eingabe` öffnet das ausgewählte Verzeichnis, und
`Rücktaste` wechselt in das übergeordnete Verzeichnis. Das Schließen
des Fensters vom Desktop aus beendet den Browser.

## EXIT STATUS

Null nach sauberem Schließen; ungleich null, wenn der Fensterkanal, die
gemeinsame Frame-Region oder die erste Verzeichnisauflistung abgelehnt
wurde (der Grund wird auf dem Standardfehlerstrom genannt).
