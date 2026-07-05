## NAME

basename — Verzeichnis und Suffix aus Namen entfernen

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

Gibt die letzte Komponente jeder Pfadschreibweise aus: abschließende
Schrägstriche werden entfernt, dann alles bis einschließlich des
letzten verbleibenden Schrägstrichs. Der Eingriff ist rein lexikalisch —
kein Pfad wird aufgelöst oder auf der Platte berührt. Mit einem
`suffix` (dem zweiten Operanden oder `-s`) wird zusätzlich ein
abschließendes `suffix` entfernt, sofern es nicht den ganzen
verbleibenden Namen ausmacht.

In eine Wurzel wird nie hineingeschnitten: `basename /` ist `/`, und —
das Gegenstück im RustOS-Speicherwald — `basename Home:/` ist `Home:/`.
Eine Alias-Wurzel (`Home:/`, `System:/`, …) spielt genau die Rolle, die
`/` auf POSIX-Systemen spielt.

Ohne `-a` oder `-s` werden höchstens zwei Operanden angenommen: der
Name und ein optionales Suffix. Mit `-a` (oder `-s`, das es impliziert)
ist jeder Operand ein Name.

## OPTIONS

- `-a, --multiple` — jeden Operanden als Namen behandeln.
- `-s, --suffix <suffix>` — ein abschließendes `suffix` von jedem Namen
  entfernen; impliziert `-a`. Auch als `--suffix=<suffix>` oder
  gebündelt (`-s.rs`) schreibbar.
- `-z, --zero` — jedes Ergebnis mit NUL statt Zeilenumbruch beenden.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `basename /System/Apps/top.app` — `top.app` ausgeben.
- `basename src/lib.rs .rs` — `lib` ausgeben.
- `basename -s .rs -a a.rs b.rs` — `a` und `b` ausgeben.
- `basename Home:/` — `Home:/` ausgeben (in eine Wurzel wird nie
  hineingeschnitten).

## EXIT STATUS

- `0` — die Ergebnisse (oder die Kurzhilfe) wurden geschrieben.
- `1` — die Ausgabe konnte nicht zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `dirname`
- `man`
