## NAME

terminal — grafischer Terminal-Emulator

## SYNOPSIS

`terminal`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das die Standard-Shell des Benutzers auf
einem Bildschirm mit 80×24 Zeichen beherbergt. In das fokussierte
Fenster getippte Tasten werden an die Shell gesendet; alles, was die
Shell schreibt (Standardausgabe wie Standardfehler), wird über das
gemeinsame ANSI/VT-Vokabular interpretiert und mit der Palette des
aktiven Themas gezeichnet. Das Terminal selbst gibt niemals ein Echo:
Echo und Zeilenbearbeitung gehören der Shell, genau wie auf einer
Konsole.

Das Terminal wird aus der Programmbibliothek des Desktops (Schaltfläche
`Library` in der Taskleiste) oder namentlich aus einer Shell gestartet.
Es benötigt eine laufende grafische Sitzung: ohne sie ist der
Fensterkanal unerreichbar, und das Terminal meldet die Ablehnung auf dem
Standardfehlerstrom und beendet sich.

Die Sitzung endet, wenn die Shell sich beendet (zum Beispiel mit
`exit`) oder wenn das Fenster vom Desktop aus geschlossen wird; das
Schließen des Fensters beendet die Shell mit einem Dateiende auf ihrer
Eingabe.

## EXIT STATUS

Null nach sauberem Schließen oder dem Beenden der Shell; ungleich
null, wenn die Shell nicht beherbergt werden konnte oder der
Fensterkanal, die gemeinsame Frame-Region oder das Ereignispostfach
abgelehnt wurde (der Grund wird auf dem Standardfehlerstrom genannt).
