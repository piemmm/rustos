## NAME

fstree — der Vollbild-Dateimanager mit Verzeichnisbaum

## SYNOPSIS

`fstree [verzeichnis]`

## DESCRIPTION

Durchsucht das Dateisystem in einer tastaturgesteuerten Vollbildsitzung:
links ein Verzeichnisbaum, rechts ein Dateibereich, der die Einträge des
ausgewählten Verzeichnisses mit Größe und Änderungszeit auflistet. Die
Sitzung beginnt in `verzeichnis` (ohne Angabe in der Wurzelansicht `/`).

Der Baum wird verzögert gelesen: Der Inhalt eines Verzeichnisses wird
erst geholt, wenn es zum ersten Mal angezeigt oder aufgeklappt wird —
das Durchstöbern eines riesigen Datenträgers kostet also nur die
tatsächlich geöffneten Verzeichnisse. Ein Verzeichnis, das der Aufrufer
nicht auflisten darf, wird an Ort und Stelle verweigert: Der Fehler
erscheint in der Meldungszeile, die vorherige Ansicht bleibt erhalten;
nichts wird erfunden.

Tasten:

- `Hoch`/`Runter` oder `k`/`j` — den Cursor des aktiven Bereichs bewegen.
  Bewegt sich der Baumcursor, wird das neu ausgewählte Verzeichnis im
  Dateibereich aufgelistet.
- `Links`/`Rechts` oder `h`/`l` — die Baumzeile unter dem Cursor
  zu-/aufklappen.
- `Eingabe` — im Baum das Aufklappen umschalten; im Dateibereich in das
  ausgewählte Verzeichnis hinabsteigen (beide Bereiche folgen).
- `Tab` — den aktiven Bereich wechseln.
- `s` — das Sortiermenü öffnen: `n` Name, `e` Erweiterung, `s` Größe,
  `m` Änderungszeit, `r` Richtung umkehren, `Esc` bricht ab.
  Verzeichnisse stehen stets vor den Dateien.
- `c` — den gewählten Eintrag kopieren: eine Eingabezeile fragt nach dem
  Ziel. Ein relatives Ziel landet im aufgelisteten Verzeichnis; ist das
  Ziel ein bestehendes Verzeichnis, wird die Kopie darin unter dem Namen
  der Quelle abgelegt. Ein Verzeichnis wird mit allem darunter kopiert.
  Das Kopieren eines Eintrags auf sich selbst oder eines Verzeichnisses
  in seinen eigenen Teilbaum wird verweigert, bevor etwas geschrieben
  wird.
- `m` — den gewählten Eintrag verschieben, mit derselben Zielabfrage.
  Innerhalb eines Datenträgers ist das Verschieben ein atomares
  Umbenennen; über Datenträgergrenzen wird kopiert und danach die Quelle
  entfernt.
- `r` — den gewählten Eintrag an Ort und Stelle umbenennen: die
  Eingabezeile ist mit dem aktuellen Namen vorbelegt.
- `d` — den gewählten Eintrag nach einer Rückfrage löschen; nur `y`
  fährt fort. Das Löschen eines Verzeichnisses entfernt alles darunter,
  und die Rückfrage sagt das.
- `M` — ein Verzeichnis im aufgelisteten Verzeichnis anlegen; der Name
  wird abgefragt.
- `a` — die Berechtigungsbits des gewählten Eintrags bearbeiten: eine
  oktale Eingabezeile, vorbelegt mit dem aktuellen Modus. Enter wendet an
  (nur der Eigentümer darf ändern — der Kernel weist alle anderen ab),
  Esc bricht ab.
- `.` — versteckte Einträge (Punktnamen) in beiden Bereichen ein- und
  ausblenden.
- `?` — diese Hilfe über den Bereichen anzeigen; jede Taste schließt sie.
- `q` — beenden und das Terminal wiederherstellen.

Würde ein Kopieren oder Verschieben eine bestehende Datei
überschreiben, fragt die Sitzung pro Datei: `o` überschreibt, `s`
überspringt (eine übersprungene Quelle bleibt an ihrem Platz), und `c`
bricht die verbleibenden Schritte ab — bereits Angewandtes bleibt
bestehen, und der Abschlussbericht sagt, was geschah. Ein Fehler
mitten im Kopieren entfernt das halb geschriebene Ziel und zeigt den
Fehler des Kernels; nichts gibt sich je als vollständige Kopie aus.
Jede Operation wird vom Kernel autorisiert — eine Verweigerung
erscheint wörtlich in der Meldungszeile, ohne dass sich etwas ändert.

Die Statuszeile zeigt den aufgelisteten Pfad, die Zahl der sichtbaren
Einträge, die Sortierordnung, die freien/gesamten Bytes des tragenden
Datenträgers (sofern der Systeminformationsdienst sie melden kann) und
ob versteckte Einträge angezeigt werden. Eine Datei, deren Format keine
Änderungszeit speichert, zeigt `-` in der Zeitspalte.

Das Markieren, die Suche und die Text-/Hex-/Disassembler-Ansichten
kommen in späteren Stufen des Plans dieses Werkzeugs.

## OPTIONS

- `directory` — das Startverzeichnis der Sitzung; Vorgabe ist die
  Wurzelansicht `/`.
- `-h`, `-?` — die Kurzform dieses Dokuments ausgeben und beenden.

## EXIT STATUS

- `0` — die Sitzung endete durch das `q` des Benutzers.
- `1` — das Startverzeichnis konnte nicht aufgelistet werden, oder der
  Terminalpfad schlug fehl.
- `2` — die Argumente konnten nicht verstanden werden.

## SEE ALSO

ls, cp, mv, rm, mkdir, chmod, du, df
