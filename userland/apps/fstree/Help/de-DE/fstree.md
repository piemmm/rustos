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
- `a` — die Berechtigungsbits des gewählten Eintrags bearbeiten: eine
  oktale Eingabezeile, vorbelegt mit dem aktuellen Modus. Enter wendet an
  (nur der Eigentümer darf ändern — der Kernel weist alle anderen ab),
  Esc bricht ab.
- `.` — versteckte Einträge (Punktnamen) in beiden Bereichen ein- und
  ausblenden.
- `?` — diese Hilfe über den Bereichen anzeigen; jede Taste schließt sie.
- `q` — beenden und das Terminal wiederherstellen.

Die Statuszeile zeigt den aufgelisteten Pfad, die Zahl der sichtbaren
Einträge, die Sortierordnung, die freien/gesamten Bytes des tragenden
Datenträgers (sofern der Systeminformationsdienst sie melden kann) und
ob versteckte Einträge angezeigt werden. Eine Datei, deren Format keine
Änderungszeit speichert, zeigt `-` in der Zeitspalte.

Die Dateioperationen (Kopieren, Verschieben, Umbenennen, Löschen), das
Markieren, die Suche und die Text-/Hex-/Disassembler-Ansichten kommen in
späteren Stufen des Plans dieses Werkzeugs.

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

ls, du, df
