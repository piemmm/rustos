## NAME

wallpaper — grafische Auswahl für den Desktop-Hintergrund

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das die vom System mitgelieferten
Hintergrundbilder, die Hintergrundfarbe dahinter und die Art und Weise
anbietet, wie der Desktop die Symbole auf seiner Pinnwand anordnet. Auf
dem Bildschirm ändert sich nichts, bis die Einstellungen angewendet
werden.

Das Fenster wird über die Maus gesteuert. Eine große Vorschau oben zeigt
das ausgewählte Hintergrundbild so, wie der Desktop es zeichnen wird,
mit der gewählten Hintergrundfarbe überall dort, wo das Bild nicht
hinkommt. Darunter listet die Galerie jedes mitgelieferte
Hintergrundbild als Kachel auf: Klicken Sie auf eine, um sie
auszuwählen, und die Vorschau folgt sofort. Die Kachel **No wallpaper**
(Kein Hintergrundbild), immer an erster Stelle, zeigt nur die gewählte
Hintergrundfarbe.

Die Galerie scrollt, wenn sie mehr Kacheln enthält, als das Fenster
anzeigt. Drehen Sie das Rad an einer beliebigen Stelle über dem Fenster,
ziehen Sie den Schieber des Scrollbalkens an der hinteren Kante oder
klicken Sie auf die Spur oberhalb oder unterhalb des Schiebers, um sich
jeweils um eine Seite zu bewegen.

Neben der Vorschau befinden sich vier Einstellungen, jeweils eine
Dropdown-Liste. Klicken Sie auf eine, um sie zu öffnen, und klicken Sie
auf eine Auswahl, um sie zu übernehmen:

- **Fit** (Einpassung) — wie das Bild platziert wird: `fill` (den
  Bildschirm ausfüllen, Überlauf abschneiden), `fit` (ganz enthalten,
  Hintergrundfarbe in den Balken), `stretch` (auf die genaue
  Bildschirmgröße verzerren), `centre` (native Größe, zentriert) und
  `tile` (ab oben links wiederholen).
- **Backdrop** (Hintergrund) — die flache Farbe, die überall dort
  angezeigt wird, wo das Hintergrundbild nicht hinkommt: `Theme` folgt
  dem aktiven Desktop-Design, und die benannten Farben sind fest. Eine
  bereits wirksame Farbe, die keine der benannten Farben ist, wird unter
  ihrer eigenen `rrggbb`-Schreibweise angeboten.
- **Icons** (Symbole) — die Ecke der Pinnwand, von der aus das
  Desktop-Symbolraster wächst.
- **Sort** (Sortierung) — die Reihenfolge, in der die Symbole des
  Desktop-Ordners aufgelistet werden.

Die Vorschau ist ein maßstabsgetreues Modell Ihres Bildschirms: Sie hat
dieselbe Form wie die Anzeige und zeigt das gewählte Bild, den
Hintergrund und die Einpassung genau so an, wie der Desktop sie
darstellen wird. Was Sie in der Vorschau sehen, entspricht dem
Ergebnis auf dem Desktop.

Hintergrundbilder werden von diesem Programm niemals dekodiert. Jedes
wird von einem separaten, sandbox-geschützten Prozess gerendert, der
keine Dateisystem-, Netzwerk- oder Startberechtigung besitzt, sodass ein
fehlerhaftes Bild weder die Auswahl noch den Desktop gefährden kann.
Eine Datei, die nicht dekodiert werden kann, wird in ihrer Kachel als
`unreadable` markiert und nicht erneut versucht.

Die Tastatur erreicht alles, was die Maus tut. `Tab` und `Shift-Tab`
bewegen den Fokus vorwärts und rückwärts durch die Galerie, die vier
Einstellungen und die beiden Schaltflächen. Die Pfeiltasten bewegen sich
innerhalb der Galerie oder öffnen die Liste der fokussierten Einstellung
und bewegen sich darin. `Enter` wendet die Einstellungen an oder
aktiviert die fokussierte Schaltfläche, und `Escape` schließt das
Fenster, ohne die Änderungen anzuwenden.

Das Anwenden sendet die gewählten Einstellungen an die Desktop-Sitzung,
die darüber entscheidet, ob sie übernommen werden, die Pinnwand neu
zeichnet und sie für die nächste Anmeldung speichert. Dieses Programm
schreibt die Einstellungen niemals selbst. Das Ergebnis wird neben den
Schaltflächen gemeldet: angewendet, mit dem Grund der Sitzung abgelehnt
oder keine Desktop-Sitzung antwortet. Eine Ablehnung lässt das Fenster
mit den getroffenen Wahlen offen.

Es wird nur der mitgelieferte Hintergrundbild-Speicher angeboten; ein
Bild an einer anderen Stelle im System kann in diesem Fenster nicht
ausgewählt werden.

## EXIT STATUS

Null nach einem sauberen Schließen, auch wenn die Einstellungen
abgelehnt wurden. Nicht Null, wenn das Fenster nicht geöffnet werden
konnte, der gemeinsame Frame-Bereich abgelehnt wurde oder der
Fensterkanal verloren ging; der Grund wird auf dem Standard-Fehlerstrom
angegeben.

## ENVIRONMENT

`HOME` benennt das eigene Heimatverzeichnis des Benutzers, unter dem
`Settings/Pinboard/pinboard.conf` beim Start gelesen wird, damit sich
das Fenster mit den aktuell gültigen Einstellungen öffnet. Dieses
Dokument wird von der Desktop-Sitzung geschrieben, niemals von diesem
Programm. Ohne `HOME` öffnet sich das Fenster mit den Standardwerten.

## SEE ALSO

`files`, `viewer`
