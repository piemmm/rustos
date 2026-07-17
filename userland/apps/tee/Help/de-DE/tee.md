## NAME

tee — von der Standardeingabe lesen und auf die Standardausgabe und in Dateien schreiben

## SYNOPSIS

`tee [option...] [datei...]`

## DESCRIPTION

Kopiert die Standardeingabe auf die Standardausgabe und in jede
genannte Datei, sodass die Daten einer Pipeline zugleich sichtbar und
festgehalten sind. Jede Datei wird angelegt, falls sie fehlt, und
überschrieben, sofern nicht `-a` anhängt. Eine Datei, die sich nicht
öffnen oder schreiben lässt, wird gemeldet, und der Lauf setzt mit den
verbleibenden Ausgaben fort — gemäß dem gewählten
`--output-error`-Modus.

TAIRiX kennt kein `SIGPIPE`: Ein verschwundener Abnehmer zeigt sich als
Schreibfehler auf der Standardausgabe — der einzigen Ausgabe dieses
Befehls, die eine Pipe sein kann —, das „Pipe" der GNU-Modi meint hier
also genau diese Ausgabe. Ohne `--output-error` beendet eine
fehlgeschlagene Standardausgabe den Lauf (das Gegenstück zum
GNU-Werkzeug, das an `SIGPIPE` stirbt, mit dem Grund auf der
Standardfehlerausgabe); mit einem `-nopipe`-Modus wird sie stillschweigend
toleriert.

GNU `tee -i` (Unterbrechungen ignorieren) ist nicht verfügbar: TAIRiX
hat keine prozessweite Signaldisposition, die sich setzen ließe. Der
Schalter kommt mit dieser Kernel-Arbeit, statt angenommen und ignoriert
zu werden.

## OPTIONS

- `-a, --append` — an die genannten Dateien anhängen; sie nicht
  überschreiben.
- `-p` — eine fehlgeschlagene Standardausgabe stillschweigend
  tolerieren; dasselbe wie `--output-error=warn-nopipe`.
- `--output-error[=<mode>]` — wie eine fehlgeschlagene Ausgabe
  behandelt wird. Ohne Wert `warn-nopipe`. Die Modi (ein eindeutiges
  Präfix wird angenommen): `warn` — einen Schreibfehler auf jeder
  Ausgabe melden, diese Ausgabe fallen lassen und fortfahren;
  `warn-nopipe` — wie `warn`, aber eine fehlgeschlagene
  Standardausgabe wird stillschweigend fallen gelassen und ändert den
  Endestatus nicht; `exit` — einen Schreibfehler auf jeder Ausgabe
  melden und anhalten; `exit-nopipe` — wie `exit`, aber eine
  fehlgeschlagene Standardausgabe wird stillschweigend fallen gelassen.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.
- `--` — die Optionsauswertung beenden; jedes weitere Argument nennt
  eine Datei, und ein Operand `-` nennt eine Datei namens `-`.

## EXAMPLES

- `ls -l | tee listing.txt` — die Auflistung anzeigen und eine Kopie
  sichern.
- `make 2>&1 | tee -a build.log` — ein Bauprotokoll anhängen und dabei
  mitlesen.
- `cat data | tee copy1 copy2 | wc -c` — zwei Kopien festhalten und
  die weiterfließenden Bytes zählen.

## EXIT STATUS

- `0` — jede Ausgabe wurde bis zum Ende der Eingabe bedient (oder die
  angeforderte Kurzhilfe wurde ausgegeben); eine von einem
  `-nopipe`-Modus tolerierte Standardausgabe ändert daran nichts.
- `1` — eine Ausgabe schlug auf eine Weise fehl, die der gewählte Modus
  zählt, oder die Eingabe konnte nicht gelesen werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `cat`
- `head`
- `wc`
