## NAME

terminal — grafischer Terminal-Emulator

## SYNOPSIS

`terminal`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das die Standard-Shell des Benutzers auf
einem Bildschirm mit 80×25 Zeichen beherbergt. In das fokussierte
Fenster getippte Tasten werden an die Shell gesendet; alles, was die
Shell schreibt (Standardausgabe wie Standardfehler), wird über das
gemeinsame ANSI/VT-Vokabular interpretiert und in dem in den
Einstellungen gewählten Farbschema gezeichnet. Das Terminal selbst
gibt niemals ein Echo: Echo und Zeilenbearbeitung gehören der Shell,
genau wie auf einer Konsole.

Das Fenster öffnet sich mit den Maßen, die der 80×25-Bildschirm bei
der jeweils geltenden Textgröße hat, sodass es auf den Bildschirm passt,
auf dem es angezeigt wird; auf einem Bildschirm, der für diese Größe zu
klein ist, wird der Text verkleinert, anstatt den Bildschirm zu
verengen, da ein Programm, das sich für 80 Spalten auslegt, diese
weiterhin erhalten muss.

Das Terminal wird aus der Programmbibliothek des Desktops (Schaltfläche
`Library` in der Taskleiste) oder namentlich aus einer Shell gestartet.
Es benötigt eine laufende grafische Sitzung: ohne sie ist der
Fensterkanal unerreichbar, und das Terminal meldet die Ablehnung auf dem
Standardfehlerstrom und beendet sich.

Die Sitzung endet, wenn die Shell sich beendet (zum Beispiel mit
`exit`) oder wenn das Fenster vom Desktop aus geschlossen wird; das
Schließen des Fensters beendet die Shell mit einem Dateiende auf ihrer
Eingabe.

Das Drücken der sekundären (rechten) Maustaste an einer beliebigen Stelle
auf dem Bildschirm öffnet das Menü des Terminals. Jede Zeile verfügt
über ein Tastenkürzel, das unabhängig davon funktioniert, ob das Menü
geöffnet ist oder nicht, und `Escape` — oder ein Klick außerhalb des
Menüs — schließt es, ohne eine Auswahl zu treffen.

| Zeile | Tastenkürzel | Funktion |
| --- | --- | --- |
| Einstellungen… | `Ctrl ,` | Öffnet die unten beschriebenen Einstellungen. |
| Größerer Text | `Ctrl +` | Zeichnet den Bildschirm eine Stufe größer. |
| Kleinerer Text | `Ctrl -` | Zeichnet den Bildschirm eine Stufe kleiner. |
| Originalgröße | `Ctrl 0` | Kehrt zur Standardtextgröße zurück. |
| Bildschirm leeren | `Ctrl Shift K` | Leert den Bildschirm, ohne an die Shell zu schreiben. |
| Schließen | `Ctrl Shift W` | Schließt das Fenster und beendet die Shell. |

Die Einstellungen öffnen sich im Fenster selbst und haben zwei
Registerkarten. **Erscheinungsbild** wählt das Farbschema aus, legt die
Textgröße fest und bearbeitet das eigene Schema des Benutzers. Die
mitgelieferten Schemata sind *System* (das dem dunklen oder hellen
Erscheinungsbild des Desktops folgt), *Midnight*, *Phosphor*, *Amber*,
*Ember*, *Contrast*, *Paper* und *Custom*. Die Wahl von *Custom*
verwendet die unter der Auswahl bearbeiteten Farben: ein Raster der
zwanzig Farben, aus denen ein Bildschirm gezeichnet wird — Hintergrund,
Vordergrund, Cursor, Cursortext und die sechzehn ANSI-Farben — mit
Schiebereglern für Rot, Grün und Blau für die jeweils ausgewählte Farbe.

**Effekte** legt fest, wie der Bildschirm gezeichnet wird.

| Effekt | Funktion |
| --- | --- |
| Deckkraft | Wie massiv der Hintergrund ist. Unter der vollen Deckkraft scheint der Desktop hinter dem Text durch, der jedoch voll lesbar bleibt. |
| Hintergrundunschärfe | Wie stark der Desktop hinter einem durchsichtigen Fenster weichgezeichnet wird. Hat keine Auswirkung auf ein vollständig deckendes Fenster. |
| Scanlinien | Dimmt abwechselnde Zeilen, der flache Teil des Aussehens einer Lochmaske. |
| Leuchten | Verteilt das Licht heller Pixel in deren Umgebung, sodass Text den weichen Lichthof einer stark ausgesteuerten Röhre trägt. |
| Rauschen | Ein sich bewegender Grundrauschpegel pro Pixel, wie ihn ein Analogsignal hat. |
| Phosphor | Wie lange leuchtende Pixel nachwirken, sodass schnell scrollender Text eine Spur hinterlässt. |
| Wackeln | Ein langsames, wanderndes horizontales Zittern, wie es eine Röhre außerhalb der Zeit hat. |

Jede Änderung wird sofort wirksam und im eigenen Profil des Benutzers
gespeichert, sodass sich ein späteres Terminal auf dieselbe Weise öffnet.
Das Betriebssystem verwahrt das Profil über seinen Einstellungsdienst, und
es ist privat für das Terminal: keine andere Anwendung kann es lesen oder
ändern. Gespeichert wird nur, was der Benutzer tatsächlich geändert hat;
*Standardwerte wiederherstellen* entfernt daher diese Entscheidungen,
anstatt die heutigen Werte einzufrieren — es gilt dann, was der
Administrator oder eine spätere Terminalversion ändert. Eine Einstellung,
die das Terminal nicht deuten kann, bleibt auf ihrem Standardwert und wird
auf dem Standardfehlerstrom gemeldet; ein nicht erreichbarer
Einstellungsdienst lässt das Terminal mit den ausgelieferten Werten
laufen, was ebenfalls gemeldet wird.

## EXIT STATUS

Null nach sauberem Schließen oder dem Beenden der Shell; ungleich
null, wenn die Shell nicht beherbergt werden konnte oder der
Fensterkanal, die gemeinsame Frame-Region oder das Ereignispostfach
abgelehnt wurde (der Grund wird auf dem Standardfehlerstrom genannt).

## ENVIRONMENT

`HOME`
: Das Heimatverzeichnis des Kontos, in dem das Terminal sein Profil
liest und schreibt. Ohne es läuft das Terminal mit dem Standardprofil
und speichert nichts.

`TERM`
: Wird der beherbergten Shell als `xterm-256color` exportiert und
benennt den Emulator, den dieses Terminal darbietet. Ein geerbter Wert
wird ersetzt; die übrige Umgebung wird unverändert an die Shell
weitergereicht.

## SEE ALSO

`elsh`, `sysinfo`
