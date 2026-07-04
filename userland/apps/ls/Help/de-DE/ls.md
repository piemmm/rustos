## NAME

ls — Verzeichnisinhalte auflisten

## SYNOPSIS

`ls [-a] [-l] [--] [path...]`

## DESCRIPTION

Listet jeden Pfad-Operanden der Reihe nach auf. Bei einem Verzeichnis
werden dessen Einträge aufgelistet, nach Namen sortiert; ein Operand,
der kein Verzeichnis ist, wird mit seinem Namen aufgelistet. Ohne
Operand wird das aktuelle Verzeichnis aufgelistet.

Einträge, deren Name mit `.` beginnt, werden ausgeblendet, sofern nicht
`-a` angegeben ist. Wenn der Standardfilter Einträge ausblendet,
vermerkt `ls` deren Anzahl auf dem Hinweisstrom (fd 3); die Auflistung
selbst bleibt unverändert.

Bei mehreren Operanden werden zuerst die Nicht-Verzeichnisse
aufgelistet (nach Namen sortiert), danach jedes Verzeichnis unter einer
`Pfad:`-Überschrift, die Blöcke durch eine Leerzeile getrennt.

Das lange Format zeigt pro Eintrag: ein Typzeichen (`d` für ein
Verzeichnis, sonst `-`), die neun Berechtigungsbits, die Größe in Byte
rechtsbündig über den Block, dann den Namen.

## OPTIONS

- `-a, --all` — Einträge, deren Name mit `.` beginnt, nicht ausblenden.
- `-l, --long` — langes Format: Typ- und Berechtigungsbits, Größe, dann
  Name.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `ls` — das aktuelle Verzeichnis auflisten.
- `ls -la /System/Apps` — jeden Eintrag von `/System/Apps`,
  einschließlich der versteckten, im langen Format auflisten.
- `ls -- -a` — die Datei oder das Verzeichnis namens `-a` auflisten.

## EXIT STATUS

- `0` — jeder Operand wurde aufgelistet.
- `1` — ein Operand konnte nicht untersucht, ein Verzeichnis nicht
  gelesen oder die Auflistung nicht ausgegeben werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `man`
