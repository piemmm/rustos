## NAME

ln — symbolische Verknüpfungen erstellen

## SYNOPSIS

`ln -s [-finvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Erstellt eine symbolische Verknüpfung, die jedes Ziel benennt. Bei
einem Operanden entsteht die Verknüpfung im Arbeitsverzeichnis unter dem
Namen des Ziels. Bei zwei ist der zweite Operand ein zu füllendes
Verzeichnis, wenn er eines ist — oder eine Verknüpfung auf eines, außer
mit `-n` — und sonst der Name der Verknüpfung. Bei drei oder mehr muss
der letzte bereits ein Verzeichnis sein.

Das Ziel wird **wortgetreu** gespeichert und niemals aufgelöst: es darf
relativ sein, `..` enthalten und überhaupt nichts benennen, eine
Verknüpfung darf also berechtigt ins Leere zeigen. Seine Grammatik wird
vor dem Speichern dennoch geprüft, sodass ein Ziel, das kein Auflöser
je durchlaufen könnte, abgewiesen wird. Das Erstellen einer Verknüpfung
verleiht keine Rechte an dem, was sie benennt — jede spätere Nutzung
wird Komponente für Komponente unter Ihrer eigenen Identität geprüft.

Ein bereits belegter Verknüpfungsname wird abgewiesen, sofern nicht `-f`
oder `-i` das Ersetzen erlaubt; das Ersetzen **entfernt** diesen Namen
zuerst, damit nichts durch eine schon vorhandene Verknüpfung hindurch
auf ihr Ziel wirkt. Ein Verzeichnis wird niemals ersetzt.

Der erste Fehlschlag beendet den Lauf vor jedem weiteren Ziel; bereits
erstellte Verknüpfungen bleiben. `--` beendet die Optionsauswertung:
jedes weitere Argument ist ein Operand.

`-s` ist auf diesem System zwingend, das keine harten Verknüpfungen
kennt: ohne `-s` gibt es nichts zu erstellen, und `ln` sagt das, statt
eine symbolische Verknüpfung anzulegen, die ein anderes Objekt ist. Die
nur für harte Verknüpfungen gedachten Optionen `-L`, `-P`, `-d` und
`-F` werden aus demselben Grund abgewiesen. `-b`/`-S` werden
abgewiesen, weil es keine Sicherungsmechanik gibt, und `-r`, weil ein
zum Verzeichnis der Verknüpfung relatives Ziel eine kanonisierende
Auflösung braucht, die dieses System nicht bietet — eine lexikalische
würde ein anderes Objekt benennen, sobald eine Verknüpfung im Spiel
ist.

## OPTIONS

- `-s, --symbolic` — symbolische Verknüpfungen erstellen. Zwingend:
  siehe oben.
- `-f, --force` — einen vorhandenen Verknüpfungsnamen entfernen und
  die Verknüpfung dann erstellen.
- `-i, --interactive` — vor dem Entfernen eines vorhandenen
  Verknüpfungsnamens fragen; nur eine mit `y`/`Y` beginnende Antwort
  stimmt zu. Die spätere von `-f` und `-i` gewinnt.
- `-n, --no-dereference` — ein Ziel, das eine symbolische Verknüpfung
  auf ein Verzeichnis ist, als den einfachen Namen behandeln, der es
  auch ist, statt als Verzeichnis für die Verknüpfungen.
- `-v, --verbose` — jede erstellte Verknüpfung als
  `'link' -> 'target'` melden.
- `-t dir, --target-directory=dir` — jede Verknüpfung in `dir`
  erstellen, das bereits ein Verzeichnis sein muss. Der Wert folgt
  angehängt (`-tdir`, `--target-directory=dir`) oder als nächstes
  Argument.
- `-T, --no-target-directory` — das Ziel als Verknüpfungsnamen
  behandeln, nie als zu füllendes Verzeichnis; genau zwei Operanden.
  Nicht mit `-t` kombinierbar.
- `-h, -?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `ln -s /System/Commands/ls.app tools/ls` — einen Namen auf ein
  Bündel verknüpfen.
- `ln -s ../shared/notes.txt` — `notes.txt` hier auf ein relatives
  Ziel verknüpfen.
- `ln -sv -t Links a.txt b.txt` — beide Dateien nach `Links`
  verknüpfen und jede Verknüpfung melden.
- `ln -sfn /Storage/media Music` — eine vorhandene `Music`-Verknüpfung
  auf ein neues Verzeichnis umlenken, also die Verknüpfung ersetzen
  statt hineinzuverknüpfen.

## EXIT STATUS

- `0` — jede Verknüpfung wurde erstellt (oder die Kurzhilfe
  geschrieben); eine abgelehnte `-i`-Frage ist kein Fehlschlag.
- `1` — alles andere, mit der Begründung auf der Standardfehlerausgabe.
  Eine nicht verstandene Befehlszeile endet ebenfalls mit `1`.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `fr-FR`).

## SEE ALSO

- `ls`
- `cp`
- `rm`
