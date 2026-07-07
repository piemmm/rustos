## NAME

vim — der modale Texteditor

## SYNOPSIS

`vim [-R] [+num | + | +/pattern] [--] [file ...]`

## DESCRIPTION

Bearbeitet Textdateien mit dem modalen Befehlssatz des bekannten
vim-Editors. Die Sitzung beginnt im Normalmodus: Tasten sind Befehle,
und `i` (oder `a`, `o` und ihre Varianten) wechselt in den
Einfügemodus, in dem Getipptes zu Text wird. `Esc` kehrt in den
Normalmodus zurück. `:q` beendet; `:wq` (oder `ZZ`) schreibt und
beendet.

Mehrere Dateien können genannt werden; die Sitzung öffnet die erste,
und `:n` / `:prev` wandern durch die Argumentliste. Eine noch nicht
vorhandene Datei ist eine `[New File]`, angelegt beim ersten Schreiben.

Befehle des Normalmodus (der umgesetzte vim-Kern):

- Bewegungen: `h j k l`, die Pfeiltasten, `w W b B e E`, `0 ^ $`,
  `f F t T` mit `;`/`,`-Wiederholung, `gg G`, `{ }`, `%`, `H M L` und
  `Enter`. Ein Zähler-Präfix wiederholt eine Bewegung: `3w`.
- Operatoren: `d` (löschen), `c` (ändern), `y` (kopieren), angewandt
  über jede Bewegung oder jedes Textobjekt (`iw aw i( a( i[ i{ i" i'
  i<` und ihre Paare); verdoppelt (`dd cc yy`) wirken sie auf ganze
  Zeilen. Kurzformen: `x X s S D C Y r ~ J`.
- Register: `"a`–`"z` vor einem Operator oder Einfügen wählt ein
  benanntes Register; Großbuchstaben hängen an. `p`/`P` fügt nach/vor
  dem Cursor ein.
- Änderungshistorie: `u` nimmt ganze Änderungen zurück, `Ctrl-R` stellt
  sie wieder her, und `.` wiederholt die letzte Änderung
  (einschließlich des eingefügten Textes).
- Suche: `/pattern` vorwärts, `?pattern` rückwärts, `n`/`N`
  wiederholen, `*` findet das Wort unter dem Cursor. Muster
  unterstützen Literale, `.`, `*`, `^`, `$`, `[...]`-Klassen und die
  Wortgrenzen `\<` `\>`. Treffer bleiben hervorgehoben bis `:noh`.
- Visuelle Auswahl: `v` (Zeichen) und `V` (Zeilen), erweitert durch
  jede Bewegung oder jedes Textobjekt, dann bearbeitet mit
  `d x c s y J`.
- Blättern: `Ctrl-D Ctrl-U` (halbes Fenster), `Ctrl-F Ctrl-B` und
  BildAuf/BildAb (ganzes Fenster); `Ctrl-G` zeigt die Dateiübersicht.

Der ex-Kern (`:`): `:w [file]`, `:q`, `:wq`, `:x`, `:e file`, `:enew`,
`:r file`, `:n`, `:prev`, `:noh`, `:set number` / `:set nonumber`,
Zeilenadressen (`:12`, `:$`, `:.+2`), `:[range]d` und
`:[range]s/pattern/replacement/[g]` (mit `&` für den ganzen Treffer in
der Ersetzung, `%` für jede Zeile des Bereichs). Ein `!` nach `w`, `q`
oder `e` erzwingt trotz Schreibschutz oder ungeschriebener Änderungen.

Alles, was vim über diesen Kern hinaus mitbringt, ist für spätere
Stufen vorgesehen; die Liste führt `plans/VIM.md` im Quellbaum.

## OPTIONS

- `-R` — schreibgeschützt: der Puffer wird im Speicher bearbeitet,
  aber `:w` wird verweigert, sofern nicht mit `:w!` erzwungen.
- `+num` — auf Zeile `num` der ersten Datei beginnen.
- `+` — auf der letzten Zeile der ersten Datei beginnen.
- `+/pattern` — auf dem ersten Treffer von `pattern` in der ersten
  Datei beginnen.
- `--` — Ende der Optionen; jedes weitere Argument ist ein Dateiname.
- `-h, -?` — die eigene Kurzhilfe dieses Befehls zeigen und beenden.

## EXIT STATUS

- `0` — die Sitzung endete mit einem Beenden-Befehl, oder die
  Kurzhilfe wurde gezeigt.
- `1` — das Terminal versagte; der Grund steht auf der
  Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Sprache der Kurzhilfe (ein BCP-47-Kennzeichen
  wie `fr-FR`).
- `TERM` — das Terminalprofil der Sitzung; unbekannte Werte fallen auf
  die einfache Grundstufe zurück.

## SEE ALSO

- `man`
- `cat`
