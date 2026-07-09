## NAME

lsusb — erkannte USB-Geräte auflisten

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Listet, eine Zeile je erkannter USB-Schnittstelle, die Bus- und
Gerätenummern der Schnittstelle, ihre `vendor:product`-Kennung sowie
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

RustOS führt kein Linux-Register für Bus-/Gerätenummern: Die Busnummer
eines Geräts ist die stabile Hardware-Baum-Knotennummer seines
Controllers, seine Gerätenummer die eigene Knotennummer, und `-s`
wählt diese Knotennummern aus (eine bewusste, dokumentierte Abweichung
vom `lsusb` unter Linux). Das Inventar führt einen Knoten je
*Schnittstelle*: Ein Gerät mit mehreren Schnittstellen erscheint einmal
je Schnittstelle.

## OPTIONS

- `-v` — nach jedem Gerät seine Schnittstellenklasse, -unterklasse und
  sein Protokoll auflisten (`bInterfaceClass`, `bInterfaceSubClass`,
  `bInterfaceProtocol`), mit den Namen der USB-Klassentabellen.
- `-t` — die Geräte als Baum unter ihren Controllern und Bussen
  darstellen.
- `-d [<vendor>]:[<product>]` — nur Geräte mit den angegebenen
  Hersteller-/Produktkennungen (hexadezimal) auflisten; eine
  ausgelassene Hälfte passt auf alles.
- `-s [[<bus>]:][<devnum>]` — nur Geräte mit den angegebenen
  Controller- (Bus-) und/oder Geräteknotennummern (dezimal) auflisten;
  ein Wert ohne Doppelpunkt ist eine Gerätenummer allein.
- `-?, --help` — die Kurzhilfe dieses Kommandos anzeigen.

## EXAMPLES

- `lsusb` — jedes erkannte USB-Gerät, mit Namen.
- `lsusb -v` — dasselbe, mit der Klassenidentität jeder Schnittstelle.
- `lsusb -s 2:` — jedes Gerät unter Controller-Knoten 2.
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
