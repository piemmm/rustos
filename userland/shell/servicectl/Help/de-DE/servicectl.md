## NAME

servicectl — Systemdienste starten und stoppen

## SYNOPSIS

`servicectl [-h | -?] start|stop SERVICE`

## DESCRIPTION

Bittet den Dienstverwalter, den Laufzeitzustand eines registrierten
Dienstes zu ändern, über seinen fähigkeitsgeschützten Steuerungsendpunkt.
Der Verwalter entscheidet: dieses Werkzeug kodiert nur die Anfrage und
meldet die Antwort.

Den Endpunkt zu erreichen ist selbst die Berechtigung. Ohne
`CAP_SERVICE_CONTROL` in der Obergrenze Ihres Kontos weist der Kernel den
Aufruf ab, bevor der Verwalter ihn sieht; ein unprivilegiertes Konto kann
also nicht einmal fragen.

- `start SERVICE` — einen registrierten, derzeit gestoppten Dienst jetzt
  starten. Die von ihm verlangten Bereitschaftsbedingungen gelten weiterhin:
  ein Dienst mit unerfüllten Bedingungen wird abgewiesen und nicht in ein
  System gestartet, das ihn nicht tragen kann.
- `stop SERVICE` — einen laufenden Dienst geordnet stoppen, samt seiner
  Abhängigen in umgekehrter Abhängigkeitsreihenfolge. Der Dienst wird zum
  Beenden aufgefordert und erst nach seiner Nachfrist erzwungen beendet.

Bei Erfolg nennt eine Zeile den Zustand, in dem der Verwalter den Dienst
hinterlassen hat.

Einen Dienst zu stoppen betrifft jeden Prinzipal der Maschine, nicht nur
Ihre eigene Sitzung, und ein eingetragener Dienst kommt beim nächsten Start
wieder: dieses Werkzeug ändert das *laufende* System, nicht das, was
aktiviert ist.

## OPTIONS

- `-h, -?` — die eigene Kurzhilfe dieses Befehls anzeigen und beenden.
- `--` — die Optionen beenden, damit ein Dienst, dessen Name mit einem
  Bindestrich beginnt, dennoch genannt werden kann.

## EXIT STATUS

- `0` — die Operation wurde angewendet, oder die Kurzhilfe wurde angezeigt.
- `1` — der Verwalter hat die Operation abgewiesen, oder der
  Steuerungsendpunkt war nicht erreichbar.
- `2` — die Befehlszeile wurde nicht verstanden; es wurde nichts gesendet.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie `fr-FR`).

## SEE ALSO

- `ps`
- `man`
