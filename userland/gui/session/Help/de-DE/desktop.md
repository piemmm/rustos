## NAME

desktop — die grafische Desktop-Sitzung starten

## SYNOPSIS

`desktop`

## DESCRIPTION

Startet die grafische Desktop-Sitzung am Arbeitsplatz dieser Maschine:
der Befehl erwirbt das exklusive Anzeige- und Eingabe-Lease des
Arbeitsplatzes, verbindet sich mit dem Anzeigedienst und führt den
komponierenden Desktop aus — den Fenstermanager und die Taskleiste —
bis die Sitzung endet. Der Befehl kehrt zurück, wenn die
Desktop-Sitzung endet.

Derselbe Desktop startet automatisch nach der Anmeldung: eine grafische
Anmeldung (`os.loginType`) ist die Vorgabe auf einer Maschine, die eine
ausführen kann. Dieser Befehl startet ihn auf Wunsch aus einer
Text-Shell.

Läuft kein Anzeigedienst, oder hält bereits eine andere Sitzung den
Arbeitsplatz, schlägt der Befehl fehl und schreibt seinen Grund auf die
Standard-Fehlerausgabe — er verdrängt niemals eine laufende Sitzung.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `desktop` — die Desktop-Sitzung starten.

## EXIT STATUS

- `0` — die Kurzhilfe wurde ausgegeben.
- `2` — die Befehlszeile wurde nicht verstanden.
- jeder andere Code ungleich null — die Sitzung konnte nicht starten
  (kein Arbeitsplatz, kein Anzeigedienst) oder endete (das
  Arbeitsplatz-Lease ging verloren); der Grund steht auf der
  Standard-Fehlerausgabe.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `fr-FR`).

## SEE ALSO

- `configure`
- `man`
