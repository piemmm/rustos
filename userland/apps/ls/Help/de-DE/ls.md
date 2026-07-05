## NAME

ls — Verzeichnisinhalte auflisten

## SYNOPSIS

`ls [-aAdFghlmnopQrRS1] [--] [path...]`

## DESCRIPTION

Listet jeden Pfad-Operanden auf: die Einträge eines
Verzeichnis-Operanden werden gelesen und aufgelistet (außer `-d`
benennt das Verzeichnis selbst), jeder andere Operand wird als er
selbst gelistet. Ohne Operand wird das aktuelle Verzeichnis (`.`)
gelistet.

Einträge werden nach Namen sortiert (oder mit `-S` nach Größe, die
größte zuerst; mit `-r` umgekehrt), standardmäßig ein Name pro Zeile.
Einträge, deren Name mit `.` beginnt, werden ausgeblendet, sofern
nicht `-a` oder `-A` angegeben ist; werden Einträge ausgeblendet,
erscheint ein Hinweis auf dem Standard-Informationsstrom (fd 3),
niemals in der Liste selbst.

Das lange Format (`-l`) zeigt die Typ- und Berechtigungsbits, den
Besitzer und die Gruppe, die Größe und dann den Namen. Besitzer und
Gruppe sind numerische Ids: das Auflösen von Kontonamen erfordert die
fähigkeitsgeschützte Benutzerdatenbank, die eine Auflistung nicht
verlangen darf; die Ausgabe entspricht daher dem numerischen
Rückfall des GNU-Werkzeugs (`-n` liefert dasselbe). Es gibt keine
Spalten für Link-Anzahl oder Zeitstempel, weil der
Dateisystem-Vertrag noch keine harten Links oder Zeitstempel trägt;
die Spalten erscheinen, sobald er es tut.

Bei mehreren Operanden — und stets unter `-R` — wird jeder
Verzeichnisliste eine `pfad:`-Kopfzeile vorangestellt, und Blöcke
werden durch eine Leerzeile getrennt.

## OPTIONS

- `-a, --all` — Einträge, deren Name mit `.` beginnt, nicht
  ausblenden.
- `-A, --almost-all` — wie `-a`, aber `.` und `..` niemals auflisten.
- `-d, --directory` — Verzeichnis-Operanden selbst auflisten, nicht
  ihren Inhalt.
- `-F, --classify` — `/` an Verzeichnisse und `*` an ausführbare
  Dateien anhängen.
- `-g` — langes Format ohne die Besitzerspalte; impliziert `-l`.
- `-h, --human-readable` — mit `-l` Größen wie `1.1K`, `23M`
  ausgeben (Potenzen von 1024).
- `-l` — langes Format: Berechtigungsbits, Besitzer, Gruppe, Größe,
  dann Name.
- `-m` — durch Kommas getrennte Namen auf einer Zeile.
- `-n, --numeric-uid-gid` — langes Format mit numerischem Besitzer
  und numerischer Gruppe; impliziert `-l`. Besitzer und Gruppe sind
  hier immer numerisch (siehe oben), also identisch zu `-l`.
- `-o` — langes Format ohne die Gruppenspalte; impliziert `-l`.
- `-p` — `/` an Verzeichnisse anhängen.
- `-Q, --quote-name` — jeden Namen in doppelte Anführungszeichen
  setzen; Anführungszeichen, Backslashes und Steuerzeichen werden
  maskiert.
- `-r, --reverse` — die Sortierreihenfolge umkehren.
- `-R, --recursive` — Unterverzeichnisse rekursiv auflisten.
- `-S` — nach Größe sortieren, die größte zuerst.
- `-1` — ein Name pro Zeile (der Standard).
- `-?` — die Kurzhilfe dieses Befehls anzeigen (`--help` ist die
  lange Form).

## EXAMPLES

- `ls` — das aktuelle Verzeichnis auflisten.
- `ls -al /System` — Auflistung von `/System` im langen Format,
  einschließlich ausgeblendeter Einträge.
- `ls -lhS` — langes Format, lesbare Größen, die größte zuerst.
- `ls -R Documents` — `Documents` rekursiv durchlaufen, eine
  Kopfzeile pro Verzeichnis.
- `ls -F` — Verzeichnisse mit `/` und ausführbare Dateien mit `*`
  markieren.
- `ls -d Documents` — den Eintrag `Documents` selbst auflisten, nicht
  seinen Inhalt.

## EXIT STATUS

- `0` — jeder Operand wurde aufgelistet.
- `1` — ein Operand konnte nicht untersucht oder ein Verzeichnis
  nicht gelesen werden, oder die Ausgabe konnte nicht zugestellt
  werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `cat`
- `man`
