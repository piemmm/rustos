## NAME

datetime — Datum und Uhrzeit des Rechners einstellen

## SYNOPSIS

`datetime`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das die Uhr des Rechners in sechs
bearbeitbaren Feldern zeigt — Jahr, Monat und Tag in der ersten Zeile,
Stunde, Minute und Sekunde in der zweiten — und stellt die Uhr auf das
ein, was dort steht. Nichts ändert sich, bis **Set** gedrückt wird.

Die Anzeige ist UTC. TAIRiX führt keinen Zeitzonen-Versatz, es gibt also
keine Ortszeit anzuzeigen und keine einzugeben.

Das Fenster wird normalerweise über das Menü der Desktop-Uhr erreicht:
auf die Uhr in der Symbolleiste klicken und **Set Date & Time…** wählen.
Das Stellen der Uhr erfordert eine Berechtigung, die eine
Desktop-Sitzung nicht besitzt; der Desktop fragt daher nach einem Konto,
das sie besitzt, und diese Anwendung wird als jenes Konto gestartet,
sobald das Kennwort angenommen wurde.

Zum Tippen ein Feld anklicken oder mit `Tab` zum nächsten wechseln.
Angenommen werden nur Ziffern, im Jahr zusätzlich ein führendes `-` für
ein Datum vor dem Jahr 1. `Enter` stellt die Uhr, `Escape` schließt das
Fenster.

Jedes Feld wird geprüft, bevor etwas gestellt wird, und der erste Fehler
wird im Fenster genannt statt still korrigiert: ein Monat außerhalb von 1
bis 12, eine Stunde außerhalb von 0 bis 23, eine Minute oder Sekunde
außerhalb von 0 bis 59 oder ein Tag, den es im eingegebenen Monat und
Jahr nicht gibt — der 31. April oder der 29. Februar außerhalb eines
Schaltjahres. Wird ein Feld abgelehnt, wird nichts gestellt.

Daten vor 1970 und weit nach 2038 sind gewöhnliche Eingaben. Die Uhr ist
ein vorzeichenbehafteter 64-Bit-Wert, keines von beiden ist eine Grenze.

Wurde die Uhr seit dem Start des Rechners noch nie gestellt, öffnen die
Felder **leer** und das Fenster sagt es. Sie werden nicht mit der
Unix-Epoche gefüllt, die ein Datum wäre, das der Rechner nie behauptet
hat.

Darf das Konto, unter dem diese Anwendung läuft, die Uhr nicht stellen,
wird der Versuch abgelehnt, das Fenster sagt es, und die Uhr bleibt
genau wie sie war. Der Grund wird zusätzlich auf den Standard-Fehlerkanal
geschrieben. Die Anwendung läuft weiter: eine abgelehnte Einstellung ist
eine Antwort, kein Fehler des Programms.

## EXIT STATUS

Null nach sauberem Schließen, auch wenn eine Einstellung abgelehnt wurde.
Nicht null, wenn das Fenster nicht geöffnet, der gemeinsame
Bildspeicherbereich abgelehnt oder der Fensterkanal verloren wurde; der
Grund steht auf dem Standard-Fehlerkanal.

## SEE ALSO

`sysinfo`, `uptime`
