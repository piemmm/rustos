## NAME

mdadm — RAID-Verbünde prüfen und verwalten

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

Prüft und verwaltet die Software-RAID-Verbünde, die der Verbund-Composer
aus Mitgliedsgeräten zusammensetzt. Der Bestand an Verbünden und Geräten
wird über die System-Informations-API gelesen — dieselbe Schnittstelle,
auf derselben `CAP_SYSINFO_HW`-Stufe, unter der auch der Hardwarebaum
gelesen wird. Die Mutationen Erstellen, Hinzufügen, Entfernen und
Stoppen werden an den Steuerungs-Endpunkt des Composers gesendet, der
vor dem Handeln prüft, dass der Aufrufer `CAP_STORAGE_ADMIN` besitzt.
Eine Ablehnung wird auf der Standardfehlerausgabe mit einem
Exit-Code ungleich null gemeldet; nichts wird erfunden und keine
Berechtigung wird angenommen.

Pro Aufruf wird genau ein Modus angegeben.

TAIRiX hat kein `/dev`, daher werden die beiden Namen, die Linux mdadm
als Gerätedateien schreibt, hier anders geschrieben — eine bewusste,
dokumentierte Abweichung:

- Ein Gerät wird durch die Knoten-ID im Hardwarebaum benannt,
  geschrieben als `node:<id>`, derselbe Name, den die Berichte anzeigen.
  Jede andere Schreibweise wird abgelehnt, statt geraten.
- Ein Verbund wird durch seine 128-Bit-Identität in Hexadezimal
  benannt. Die vollständige 32-stellige Identität wird akzeptiert,
  ebenso jedes Präfix, das genau einen Verbund benennt; ein Präfix, das
  auf mehr als einen Verbund passt, wird abgelehnt, statt zu raten,
  welcher gemeint war.

TAIRiX setzt die RAID-Level 0, 1, 5, 6, 10 und dreifache Parität
zusammen. Es gibt kein RAID4, daher wird `--level=4` mit dieser
Begründung abgelehnt.

Knapper beratender Kontext — ein degradierter Verbund oder in der
Verbund-Ansicht nicht gezeigte leere Geräte — wird auf den
Standard-Informationsstrom (fd 3) geschrieben. Er ist optional und
ändert nie die primäre Ausgabe.

## OPTIONS

- `-C, --create` — einen Verbund über die genannten Geräte erstellen und
  die Identität ausgeben, die der Composer ihm vergibt.
- `-D, --detail` — Identität, Level, Zustand, Gerätezahlen, Geometrie
  und jede laufende Wiederherstellungs- oder Prüfposition jedes Verbunds
  melden. Ohne Verbund-Operanden jeden Verbund melden.
- `-E, --examine` — jedes Gerät auflisten, das der Composer hält:
  Verbund-Mitglieder mit ihrem Platz und Zustand sowie die
  nicht zugeordneten leeren Geräte, über die ein neuer Verbund erstellt
  werden kann.
- `-a, --add` — ein leeres Gerät in einen fehlenden Platz eines Verbunds
  aufnehmen und es wiederherstellen.
- `-r, --remove` — ein Mitgliedsgerät aus einem Verbund ausmustern.
- `-S, --stop` — einen aktiven Verbund stoppen und seine Mitglieder
  freigeben.
- `-l, --level=<level>` — der zu erstellende Level: `0`/`raid0`/`stripe`,
  `1`/`raid1`/`mirror`, `5`/`raid5`, `6`/`raid6`, `10`/`raid10`, oder
  `tp`/`raid-tp` für dreifache Parität.
- `-n, --raid-devices=<count>` — die Anzahl der zu erstellenden
  Mitgliedsplätze; sie muss der Anzahl der Geräte-Operanden entsprechen.
- `-c, --chunk=<blocks>` — die Streifeneinheit in logischen Blöcken;
  nur für einen gestreiften Level gültig.
- `-h, -?, --help` — die eigene Hilfe dieses Befehls anzeigen.
- `-V, --version` — die Version ausgeben und beenden.

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — einen RAID5-Verbund über drei Geräte erstellen.
- `mdadm --detail` — jeden Verbund melden.
- `mdadm --examine` — jedes Gerät auflisten, Mitglieder wie leere.
- `mdadm --add 3f2a node:14` — ein Gerät zum Verbund hinzufügen, dessen Identität mit `3f2a` beginnt.
- `mdadm --stop 3f2a` — diesen Verbund stoppen.

## EXIT STATUS

- `0` — die Anfrage war erfolgreich (oder die Hilfe wurde geschrieben).
- `1` — eine Berechtigung wurde verweigert, ein Name ließ sich nicht
  auflösen, der Composer lehnte die Anfrage ab, oder die Ausgabe konnte
  nicht geschrieben werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für diese Hilfe (ein BCP-47-Kürzel wie
  `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
