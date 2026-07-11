## NAME

lsusb — erkannte USB-Geräte auflisten

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Listet, eine Zeile je erkanntem USB-Gerät, die Bus- und
Gerätenummern des Geräts, seine `vendor:product`-Kennung sowie
die Namen von Hersteller und Produkt. Das Inventar ist der
Hardware-Baum — das einzige Geräteinventar des Systems — gelesen über
die Systeminformations-API, die die Capability `CAP_SYSINFO_HW`
verlangt; eine Ablehnung wird auf der Standardfehlerausgabe gemeldet,
und an ihrer Stelle wird nichts aufgelistet.

Die Namen stammen aus dem geprüften Abzug der öffentlichen
USB-ID-Datenbank, den dieses Kommando in seinem eigenen Paket
mitführt. Eine Kennung, die die Datenbank nicht benennt, zeigt nur ihre
numerische Form `ID vvvv:pppp`, niemals eine erfundene, und die Anzahl
solcher Geräte wird auf dem Standardinformationsstrom (fd 3) vermerkt.
Fehlt die mitgelieferte Tabelle oder scheitert ihre Prüfung, fällt die
Auflistung auf nackte Kennungen zurück, mit dem Grund auf der
Standardfehlerausgabe — das Inventar selbst wird weiterhin gelistet.

RustOS führt kein Linux-Register für Bus-/Gerätenummern: Bus- und
Gerätenummern sind kleine, bei 1 beginnende Ordnungszahlen über das
aktuelle Inventar (Busse in Erkennungsreihenfolge, Geräte in
Auflistungsreihenfolge je Bus), stabil solange sich die Topologie nicht
ändert, und `-s` wählt diese angezeigten Nummern aus (eine bewusste,
dokumentierte Abweichung vom `lsusb` unter Linux). Das Inventar führt
einen Eintrag je *Schnittstelle*: Die Schnittstellen eines physischen
Geräts werden anhand der vom Host-Controller gemeldeten Geräteadresse
gruppiert, sodass ein Gerät mit mehreren Schnittstellen nur einmal
erscheint.

## OPTIONS

- `-v` — nach jedem Gerät für jede seiner Schnittstellen Klasse,
  Unterklasse und Protokoll auflisten (`bInterfaceClass`,
  `bInterfaceSubClass`, `bInterfaceProtocol`), mit den Namen der
  USB-Klassentabellen.
- `-t` — die Busse, ihre Geräte und die Schnittstellenklassen jedes
  Geräts als Baum darstellen.
- `-d [<vendor>]:[<product>]` — nur Geräte mit den angegebenen
  Hersteller-/Produktkennungen (hexadezimal) auflisten; eine
  ausgelassene Hälfte passt auf alles.
- `-s [[<bus>]:][<devnum>]` — nur Geräte mit den angegebenen Bus-
  und/oder Gerätenummern (dezimal) auflisten, wie sie in der Auflistung
  erscheinen; ein Wert ohne Doppelpunkt ist eine Gerätenummer allein.
- `-?, --help` — die Kurzhilfe dieses Kommandos anzeigen.

## EXAMPLES

- `lsusb` — jedes erkannte USB-Gerät, mit Namen.
- `lsusb -v` — dasselbe, mit der Klassenidentität jeder Schnittstelle.
- `lsusb -s 2:` — jedes Gerät auf Bus 2.
- `lsusb -d 046d:` — jedes Gerät des Herstellers `046d` (Logitech).
- `lsusb -t` — die Geräte in ihrer Bus-Topologie.

## EXIT STATUS

- `0` — die Auflistung (oder die Kurzhilfe) wurde geschrieben.
- `1` — die Hardware-Baum-Abfrage wurde abgelehnt oder schlug fehl,
  oder die Ausgabe konnte nicht geschrieben werden.
- `2` — die Kommandozeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `lspci`
- `sysinfo`
- `man`
