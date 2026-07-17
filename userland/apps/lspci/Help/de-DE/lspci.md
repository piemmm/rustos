## NAME

lspci — erkannte PCI/PCIe-Geräte auflisten

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Listet, eine Zeile je erkannter PCI/PCIe-Funktion, die
Hardwarebaum-Knotennummer der Funktion, ihre Klasse sowie Hersteller-
und Gerätenamen. Das Inventar ist der Hardwarebaum — das einzige
Geräteinventar des Systems — gelesen über die
Systeminformations-API, die die Capability `CAP_SYSINFO_HW` verlangt;
eine Ablehnung wird auf der Standardfehlerausgabe gemeldet, und an
ihrer Stelle wird nichts aufgelistet.

Die Namen stammen aus dem geprüften Abzug der öffentlichen
PCI-ID-Datenbank, den dieses Kommando im eigenen Bündel mitführt. Eine
Identität, die die Datenbank nicht benennt, erscheint in numerischer
Form (`Vendor 8086`, `Device 2922`, `Class 0106`), nie erfunden; die
Anzahl solcher Geräte wird auf dem Standardinformationsstrom (fd 3)
vermerkt. Fehlt die mitgeführte Tabelle oder besteht sie die Prüfung
nicht, fällt die Auflistung auf numerische Kennungen zurück, mit der
Begründung auf der Standardfehlerausgabe — das Inventar selbst wird
weiterhin aufgelistet.

TAIRiX führt keine PCI-Adresse `bus:device.function`: Die stabile
Adresse einer Funktion ist ihre Hardwarebaum-Knotennummer, angezeigt
als `#<node>`, und `-s` wählt diese Nummer aus (eine bewusste,
dokumentierte Abweichung vom Linux-`lspci`). Die `-k`-Ansicht
(Kerneltreiber) wird noch nicht angeboten: Das System veröffentlicht
keine Treiberbindungs-Datensätze, und `lspci` meldet nur, was das
System tatsächlich verzeichnet.

## OPTIONS

- `-n` — nur numerische Kennungen: der Klassencode und
  `vendor:device` hexadezimal.
- `-nn` — Namen, gefolgt von den numerischen Kennungen in Klammern.
- `-v` — nach jeder Funktion die Ressourcen auflisten, die ihr Knoten
  deklariert (MMIO-Fenster, IRQ-Leitungen, E/A-Ports,
  DMA-Beschränkungen) — die verzeichneten Capability-Anforderungen,
  kein Live-Zustand.
- `-t` — die Funktionen als Baum unter ihren Bus-Eltern darstellen.
- `-d [<vendor>]:[<device>]` — nur Funktionen mit den angegebenen
  Kennungen (hexadezimal) auflisten; eine ausgelassene Hälfte passt
  auf alles.
- `-s <node>` — nur die Funktion mit der angegebenen
  Hardwarebaum-Knotennummer (dezimal) auflisten.
- `-?, --help` — die Kurzhilfe dieses Kommandos anzeigen.

## EXAMPLES

- `lspci` — jede erkannte PCI-Funktion, mit Namen.
- `lspci -nn` — dasselbe, mit den numerischen Kennungen daneben.
- `lspci -v -s 7` — die Zeile von Knoten 7 samt deklarierter
  Ressourcen.
- `lspci -d 1af4:` — jede Funktion des Herstellers `1af4` (virtio).
- `lspci -t` — die Funktionen unter ihrer Bus-Topologie.

## EXIT STATUS

- `0` — die Auflistung (oder die Kurzhilfe) wurde geschrieben.
- `1` — die Hardwarebaum-Abfrage wurde abgelehnt oder schlug fehl,
  oder die Ausgabe konnte nicht geschrieben werden.
- `2` — die Kommandozeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `sysinfo`
- `man`
