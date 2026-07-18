## NAME

ls — Verzeichnisinhalte auflisten

## SYNOPSIS

`ls [-aACcdFfghilmnopQrRsStUuvXx1] [-w cols] [--time=WORD]`
`[--time-style=STYLE] [--sort=WORD] [--full-time]`
`[--group-directories-first] [--] [path...]`

## DESCRIPTION

Listet jeden Pfad-Operanden auf: die Einträge eines
Verzeichnis-Operanden werden gelesen und aufgelistet (außer `-d`
benennt das Verzeichnis selbst), jeder andere Operand wird als er
selbst gelistet. Ohne Operand wird das aktuelle Verzeichnis (`.`)
gelistet.

Einträge werden nach Namen sortiert (oder mit `-S` nach Größe, die
größte zuerst; mit `-t` nach Zeit, die neueste zuerst; mit `-r`
umgekehrt), standardmäßig ein Name pro Zeile.
Einträge, deren Name mit `.` beginnt, werden ausgeblendet, sofern
nicht `-a` oder `-A` angegeben ist; werden Einträge ausgeblendet,
erscheint ein Hinweis auf dem Standard-Informationsstrom (fd 3),
niemals in der Liste selbst.

Das lange Format (`-l`) zeigt die Typ- und Berechtigungsbits, den
Besitzer und die Gruppe, die Größe und dann den Namen. Besitzer und
Gruppe sind numerische Ids: das Auflösen von Kontonamen erfordert die
fähigkeitsgeschützte Benutzerdatenbank, die eine Auflistung nicht
verlangen darf; die Ausgabe entspricht daher dem numerischen
Rückfall des GNU-Werkzeugs (`-n` liefert dasselbe). Die
Zeitstempelspalte zeigt standardmäßig die Änderungszeit; `-c`, `-u`
und `--time` wählen, welcher der vier Zeitstempel angezeigt (und
wonach sortiert) wird, und `--time-style` — oder `--full-time` — legt
sein Format fest. Es gibt weiterhin keine Spalte für die Link-Anzahl,
weil der Dateisystem-Vertrag noch keine harten Links trägt; sie
erscheint, sobald er es tut.

Bei mehreren Operanden — und stets unter `-R` — wird jeder
Verzeichnisliste eine `pfad:`-Kopfzeile vorangestellt, und Blöcke
werden durch eine Leerzeile getrennt.

## OPTIONS

- `-t` — nach dem angezeigten Zeitstempel sortieren, den neuesten
  zuerst.
- `-c` — die Metadaten-Änderungszeit (ctime) verwenden: mit `-l`
  anzeigen und mit `-t` danach sortieren; ohne `-l` danach sortieren.
- `-u` — wie `-c`, aber die Zugriffszeit (atime).
- `-i, --inode` — die Knotennummer jedes Eintrags ausgeben.
- `--time=WORD` — welcher Zeitstempel angezeigt und wonach sortiert
  wird: `atime` (`access`, `use`), `ctime` (`status`), `mtime`
  (`modification`) oder `birth` (`creation`).
- `--time-style=STYLE` — Zeitstempelformat: `locale` (Standard),
  `long-iso`, `full-iso` oder `iso`. Ein eigenes `+FORMAT` wird nicht
  unterstützt.
- `--full-time` — wie `-l --time-style=full-iso`.
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
- `-m` — durch Kommas getrennte Namen, auf die Breite umgebrochen.
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
- `-s, --size` — die belegte Größe jedes Eintrags in 1024-Byte-Blöcken
  ausgeben (mit `-h` skaliert), mit einer `total`-Zeile je aufgelistetem
  Verzeichnis.
- `-C` — Einträge in Spalten auflisten, von oben nach unten gefüllt
  (Standard am Terminal).
- `-S` — nach Größe sortieren, die größte zuerst.
- `-U` — nicht sortieren; Einträge in Verzeichnisreihenfolge auflisten.
- `-X` — nach Dateiendung (Text ab dem letzten `.`) sortieren, bei
  Gleichstand nach Namen.
- `-v` — natürliche „Versions“-Sortierung, sodass `f2` vor `f10`
  steht; bei Gleichstand nach Namen.
- `-f` — nicht sortieren und alle Einträge zeigen: aktiviert `-a` und
  `-U` und deaktiviert `-l` und `-s`. Wirkt an seiner Position, sodass
  ein späteres `-l`/`-s`/Sortier-Flag es überschreibt.
- `--sort=WORD` — Sortierschlüssel nach Name wählen: `none` (`-U`),
  `size` (`-S`), `time` (`-t`), `version` (`-v`), `extension` (`-X`)
  oder `name`.
- `--group-directories-first` — Verzeichnisse vor anderen Einträgen
  auflisten; Verzeichnisse zuerst, auch mit `-r`.
- `-w, --width <cols>` — die Ausgabebreite in Spalten festlegen;
  `0` bedeutet unbegrenzt.
- `-x` — Einträge in Spalten auflisten, von links nach rechts gefüllt.
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
