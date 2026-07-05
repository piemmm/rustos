## NAME

rm — Dateien und Verzeichnisse entfernen

## SYNOPSIS

`rm [-dfiIrRv] [--] file...`

## DESCRIPTION

Entfernt jeden Dateioperanden, der Reihe nach. Ein Operand, der kein
Verzeichnis ist, wird entlinkt; ein Verzeichnisoperand wird nur mit
`-r` entfernt (das den Inhalt tiefenzuerst und dann das Verzeichnis
selbst entfernt) oder, wenn er leer ist, mit `-d`.

Mit `-f` wird ein nicht vorhandener Operand stillschweigend
übersprungen und nie eine Frage gestellt. `-i` fragt auf der
Standardfehlerausgabe vor jeder Entfernung und vor dem Abstieg in ein
Verzeichnis; `-I` fragt einmal vorab, bevor mehr als drei Operanden
oder rekursiv entfernt wird. Eine abgelehnte Frage überspringt das
Objekt (oder den ganzen Lauf, bei `-I`) ohne Fehler; eine unlesbare
Antwort gilt nie als Zustimmung. Das spätere von `-f`, `-i` und `-I`
gewinnt.

Der Operand `/` wird unter `--preserve-root`, dem Standard,
abgelehnt. Der erste Fehlschlag beendet den Lauf vor jedem späteren
Operanden. `--` beendet die Optionsauswertung: jedes spätere Argument
ist ein Pfad.

## OPTIONS

- `-r, -R, --recursive` — Verzeichnisse mitsamt Inhalt entfernen.
- `-f, --force` — nicht vorhandene Operanden ignorieren; nie fragen.
- `-d, --dir` — leere Verzeichnisse entfernen.
- `-i, --interactive` — vor jeder Entfernung fragen; nur eine mit
  `y`/`Y` beginnende Antwort stimmt zu.
- `-I` — einmal fragen, bevor mehr als drei Operanden oder rekursiv
  entfernt wird.
- `-v, --verbose` — jede Entfernung als `removed 'file'` melden.
- `--preserve-root` — das Entfernen von `/` verweigern (der
  Standard).
- `--no-preserve-root` — das Entfernen von `/` erlauben.
- `-h, -?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `rm notes.txt` — eine Datei entfernen.
- `rm -r Scratch` — den Baum `Scratch` samt Inhalt entfernen.
- `rm -I a b c d` — einmal fragen, dann bei `y` alle vier Dateien
  entfernen.

## EXIT STATUS

- `0` — jede Entfernung ist gelungen (eine abgelehnte Frage und ein
  `-f`-Überspringen sind keine Fehlschläge).
- `1` — ein Dateisystem-, Frage- oder Ausgabefehler; der Grund wird
  auf der Standardfehlerausgabe gemeldet.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `cp`
- `ls`
- `mv`
