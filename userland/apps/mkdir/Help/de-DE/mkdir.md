## NAME

mkdir — Verzeichnisse anlegen

## SYNOPSIS

`mkdir [-pv] [--] Verzeichnis...`

## DESCRIPTION

Legt jeden Verzeichnis-Operanden der Reihe nach an. Ohne `-p` muss das
Elternverzeichnis jedes Operanden bereits existieren und der Operand
selbst darf nicht existieren; der erste Fehlschlag beendet den Lauf vor
jedem weiteren Operanden.

Mit `-p` wird jeder fehlende Vorfahr zuerst angelegt, vom äußersten zum
innersten, und ein Operand (oder Vorfahr), der bereits als Verzeichnis
existiert, ist kein Fehler. Ein Vorfahr, der als Datei existiert,
schlägt weiterhin fehl: nichts wird jemals stillschweigend ersetzt.

Die Option `-m`/`--mode` von GNU `mkdir` wird noch nicht akzeptiert:
Verzeichnisse werden mit dem Standardmodus des Dateisystems angelegt,
bis der Mechanismus zum Setzen von Modi verfügbar ist; die Option kommt
mit ihm, statt ignoriert zu werden. `--` beendet die Optionsanalyse:
jedes spätere Argument ist ein Pfad.

## OPTIONS

- `-p, --parents` — fehlende Elternverzeichnisse anlegen; ein Operand,
  der bereits ein Verzeichnis ist, ist kein Fehler.
- `-v, --verbose` — jedes angelegte Verzeichnis als
  `mkdir: created directory 'Verz'` melden.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen (auch `--help`).

## EXAMPLES

- `mkdir Notes` — ein Verzeichnis im aktuellen Verzeichnis anlegen.
- `mkdir -p Projects/os/build` — die ganze Kette anlegen und bereits
  vorhandene Teile überspringen.
- `mkdir -pv Home:/tools/bin` — unter einer Alias-Wurzel anlegen und
  jedes neue Verzeichnis melden.

## EXIT STATUS

- `0` — jedes Verzeichnis wurde angelegt (oder existierte mit `-p`
  bereits).
- `1` — ein Dateisystem- oder Ausgabefehler; der Grund wird auf der
  Standardfehlerausgabe gemeldet.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

rmdir, rm, ls
