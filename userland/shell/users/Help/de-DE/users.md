## NAME

users — Benutzerkonten und Gruppen verwalten

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

Startet die interaktive Kontenverwaltungssitzung über die geprüfte
`users_admin`-Schnittstelle. Jede Operation wird kernelseitig anhand
der vom Kernel bezeugten Identität entschieden: ohne `CAP_USER_ADMIN`
in der Obergrenze des Kontos wird jede Operation bei der Zustellung
verweigert. Passwörter werden mit abgeschaltetem Terminal-Echo gelesen
und clientseitig in einen gesalzenen Datensatz gehasht; Klartext
überquert die Schnittstelle nie und wird nie angezeigt oder
protokolliert.

Das Werkzeug nimmt keine Operanden an: Konten werden mit Befehlen
verwaltet, die innerhalb der Sitzung eingegeben werden.

- `list` — Benutzerkonten auflisten.
- `groups` — Gruppen auflisten.
- `create <name> <uid> <gid>` — ein Konto anlegen.
- `passwd <name>` — das Passwort eines Kontos setzen.
- `lock <name>`, `unlock <name>` — ein Konto sperren oder wieder
  freigeben.
- `grant <name> <CAP_...>`, `revoke <name> <CAP_...>` — die einem Konto
  gewährten Capabilities bearbeiten.
- `deluser <name>` — ein Konto löschen.
- `addgroup`, `delgroup` — eine Gruppe anlegen oder löschen.
- `help` — die Sitzungsbefehle auflisten.
- `exit`, `quit` — die Sitzung beenden.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen und beenden.

## EXIT STATUS

- `0` — die Sitzung endete sauber, oder die Kurzhilfe wurde angezeigt.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `man`
