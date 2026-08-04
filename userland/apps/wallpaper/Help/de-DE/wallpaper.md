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

Das Raster listet jedes mitgelieferte Hintergrundbild als Miniaturansicht
auf, plus einen Eintrag **No wallpaper** (Kein Hintergrundbild), der nur
die gewählte Hintergrundfarbe anzeigt. Jede Miniaturansicht wird unter
der aktuell gewählten Einpassung gerendert, sodass eine Vorschau zeigt,
was der Desktop tatsächlich mit diesem Bild tun wird. Eine Datei, die
nicht dekodiert werden kann, zeigt eine markierte Platzhalterkachel mit
ihrem Namen an und wird nicht erneut versucht.

Hintergrundbilder werden von diesem Programm niemals dekodiert. Jedes
wird von einem separaten, sandbox-geschützten Prozess gerendert, der
keine Dateisystem-, Netzwerk- oder Startberechtigung besitzt, sodass ein
fehlerhaftes Bild weder die Auswahl noch den Desktop gefährden kann.

Die Optionszeilen unter dem Raster sind:

- **Fit** (Anpassung) — wie das Bild platziert wird: `fill` (den
  Bildschirm ausfüllen, Überlauf abschneiden), `fit` (ganz enthalten,
  Hintergrundfarbe in den Balken), `stretch` (auf die genaue
  Bildschirmgröße verzerren), `centre` (native Größe, zentriert) und
  `tile` (ab oben links wiederholen).
- **Backdrop** (Hintergrund) — die flache Farbe, die überall dort
  angezeigt wird, wo das Hintergrundbild nicht hinkommt: `Theme` folgt
  dem aktiven Desktop-Design, und die benannten Farben sind fest. Eine
  bereits wirksame Farbe, die keine der benannten Farben ist, wird unter
  ihrer eigenen `rrggbb`-Schreibweise angeboten.
- **Icons** (Symbole) — die Seite der Pinnwand, von der aus das
  Desktop-Symbolraster wächst.
- **Sort** (Sortierung) — die Reihenfolge, in der die Symbole des
  Desktop-Ordners aufgelistet werden.

Das Fenster wird über die Tastatur gesteuert. `Tab` und `Shift-Tab`
bewegen den Fokus vorwärts und rückwärts durch das Raster, die
Optionszeilen und die Schaltflächen. Die Pfeiltasten bewegen sich
innerhalb des Miniaturrasters oder ändern die fokussierte Option.
`Enter` aktiviert die fokussierte Schaltfläche, und `Escape` schließt
das Fenster, ohne die Änderungen anzuwenden.

Das Anwenden sendet die gewählten Einstellungen an die Desktop-Sitzung,
die darüber entscheidet, ob sie übernommen werden, die Pinnwand neu
zeichnet und sie für die nächste Anmeldung speichert. Dieses Programm
schreibt die Einstellungen niemals selbst. Das Ergebnis wird in der
Statuszeile unter den Optionszeilen gemeldet: angewendet, mit dem Grund
der Sitzung abgelehnt oder keine Desktop-Sitzung antwortet. Eine
Ablehnung lässt das Fenster mit den getroffenen Wahlen offen.

Es wird nur der mitgelieferte Hintergrundbild-Speicher angeboten; ein
Bild an einer anderen Stelle im System kann in diesem Fenster nicht
ausgewählt werden. Mausklicks wählen nichts aus.

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
