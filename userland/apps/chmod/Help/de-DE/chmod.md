## NAME

chmod — Dateimodusbits ändern

## SYNOPSIS

`chmod [-cfRv] [--] MODE file...`

## DESCRIPTION

Ändert die Berechtigungsbits jedes Dateioperanden auf `MODE`, der
Reihe nach. `MODE` ist entweder ein absoluter Oktalwert (`644`,
`0755`, …), der die Berechtigungsbits vollständig ersetzt, oder eine
kommagetrennte Liste symbolischer Klauseln `[ugoa]*[-+=][rwxXst]*`
(`g+w`, `o-rx`, `a=rx`, `u+s`), die die aktuellen Bits der Datei
umformen. Das symbolische `X` gewährt Ausführen nur einem Verzeichnis
oder einer Datei, die bereits ein Ausführungsbit trägt.

Nur der Eigentümer einer Datei darf ihren Modus ändern; der Kernel
weist jeden anderen ab, und der Besitz einer Capability gewährt kein
Vorrecht. Mit `-R` wird ein Verzeichnisoperand geändert und danach
sein Inhalt rekursiv. Der erste Fehlschlag beendet den Lauf vor jedem
späteren Operanden. `--` beendet die Optionsauswertung: jedes spätere
Argument ist ein Operand. Ein mit `-` beginnender Modus wird ohne den
Strich geschrieben (`a-w`), oder die Optionen werden zuerst beendet
(`chmod -- -w file`).

## OPTIONS

- `-R, --recursive` — Dateien und Verzeichnisse rekursiv ändern.
- `-c, --changes` — nur Dateien melden, deren Modus sich tatsächlich
  geändert hat.
- `-v, --verbose` — jede verarbeitete Datei melden.
- `-f, --silent, --quiet` — die meisten Fehlermeldungen
  unterdrücken; der Lauf schlägt dennoch fehl, und der Exit-Status
  meldet es.
- `-h, -?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `chmod 644 notes.txt` — Eigentümer lesen/schreiben, alle anderen
  nur lesen.
- `chmod g+w shared.txt` — der Gruppe Schreibrecht zu den aktuellen
  Bits hinzufügen.
- `chmod -R a=rx Docs` — den Baum `Docs` für alle lesbar und
  betretbar machen.

## EXIT STATUS

- `0` — jede Modusänderung ist gelungen.
- `1` — ein Dateisystem- oder Ausgabefehler; der Grund wird auf der
  Standardfehlerausgabe gemeldet (unter `-f` unterdrückt).
- `2` — die Befehlszeile wurde nicht verstanden, oder der
  Modusoperand war weder oktal noch symbolisch.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `ls`
- `mkdir`
- `rm`
