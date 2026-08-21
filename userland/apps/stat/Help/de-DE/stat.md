## NAME

stat — den Status einer Datei oder eines Dateisystems ausgeben

## SYNOPSIS

`stat [-Lft] [-c FORMAT | --printf=FORMAT] [--] Datei...`

## DESCRIPTION

Gibt die Felder je eines gelesenen Status pro Operand aus, in der
Reihenfolge der Befehlszeile.

**Ohne `-L` wird eine symbolische Verknüpfung als sie selbst
beschrieben** — dafür ist dieses Werkzeug neben `ls` da. `%N` zeigt die
Verknüpfung und das von ihr gespeicherte Ziel, `%F` sagt
`symbolic link`, und Größen und Zeitstempel sind die der Verknüpfung
selbst. `-L` löst die letzte Verknüpfung auf und beschreibt, was sie
benennt.

`-f` wechselt zum Dateisystem, auf dem der Operand liegt: die Block- und
Inode-Zahlen des Datenträgers, seine Blockgröße und den Typ, den seine
Einhängung festhält. Die beiden Lesarten haben **verschiedene**
Feldvokabulare, daher wird ein Format gegen das von `-f` gewählte
geprüft.

`-c`/`--format` gibt eine Formatzeichenkette je Operand aus, gefolgt von
einem Zeilenumbruch; `--printf` deutet Rückschrägstrich-Escapes und
hängt keinen Umbruch an. Das ist der einzige Unterschied. Eine Direktive
nimmt die printf-Flags und -Breite (`%-10s`, `%06i`, `%.3n`), damit ein
Bericht in Spalten stehen kann. `-t` ist die einzeilige knappe Form
beider Lesarten.

Ein Operand, der nicht gelesen werden kann, wird auf der
Standardfehlerausgabe gemeldet, die übrigen Operanden werden weiterhin
beschrieben, und der Befehl endet mit einem Status ungleich null. Ein
Feld, das dieses System nicht liefern kann — eine Einhängungsübersicht,
die es nicht lesen darf, eine uid ohne Namen im Benutzerverzeichnis —
erscheint als `?` oder als GNUs `UNKNOWN`, niemals als plausibler Ersatz.

Mindestens ein Operand ist erforderlich. `--` beendet die
Optionsauswertung.

Vier Felder benennen einen Begriff, den TAIRiX nicht hat, und werden
namentlich **abgewiesen**, wenn ein Format eines davon verwendet, statt
mit einem erfundenen Wert beantwortet zu werden: `%G`, weil die System
Information API ein Benutzerverzeichnis und kein Gruppengegenstück
veröffentlicht, sodass `%g` (die numerische Kennung) das ehrliche Feld
ist; `%t` und `%T` des Datei-Vokabulars, weil es keine
Gerätespezialdateien gibt, die einen Haupt- oder Nebentyp hätten; und
`%t` des Dateisystem-Vokabulars, weil ein Datenträger keine numerische
Typkennung hat — `%T` benennt den Typ, den seine Einhängung festhält.
Die Abweisung erfolgt beim Auswerten des Formats, bevor ein Pfad berührt
wird.

Zwei Felder berichten einen TAIRiX-Begriff anstelle eines
Linux-Begriffs. Ein Datenträger wird durch eine 16-Byte-Kennung statt
durch eine Gerätenummer bezeichnet, also ist `%d` diese Kennung dezimal
und `%D` hexadezimal; ein Vergleich der `%d` zweier Dateien beantwortet
weiterhin genau „liegen diese auf einem Datenträger?".

## OPTIONS

- `-L, --dereference` — beschreiben, was eine symbolische Verknüpfung
  benennt, statt der Verknüpfung selbst.
- `-f, --file-system` — das Dateisystem beschreiben, das den Operanden
  hält, statt den Operanden.
- `-c, --format=FORMAT` — `FORMAT` je Operand ausgeben, gefolgt von einem
  Zeilenumbruch.
- `--printf=FORMAT` — wie `-c`, aber Rückschrägstrich-Escapes deuten und
  keinen abschließenden Umbruch ausgeben.
- `-t, --terse` — die Felder in einer leergetrennten Zeile ausgeben.
- `-?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `stat notes.txt` — der vollständige Bericht für eine Datei.
- `stat -c '%s %n' *` — Größe und Name, je eine Zeile.
- `stat -L link` — beschreiben, was die Verknüpfung benennt.
- `stat -f .` — der Datenträger des Arbeitsverzeichnisses.

## EXIT STATUS

- `0` — jeder Operand wurde beschrieben (oder die Kurzhilfe geschrieben).
- `1` — mindestens ein Operand war nicht lesbar, oder die Ausgabe ist
  fehlgeschlagen.
- `2` — die Befehlszeile wurde nicht verstanden, oder ihr Format nannte
  eine Direktive, die dieses System nicht bedienen kann.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein
  BCP-47-Kennzeichen wie `fr-FR`).

## SEE ALSO

ls, readlink, df, du
