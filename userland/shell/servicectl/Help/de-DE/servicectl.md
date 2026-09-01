## NAME

servicectl — Systemdienste starten, stoppen, ein- und austragen

## SYNOPSIS

`servicectl [-h | -?] start|stop|enable|disable SERVICE`

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
- `enable SERVICE` — den Dienst als eingetragen vermerken, sodass der
  Verwalter ihn bei jedem Start hochfährt, und ihn jetzt starten.
- `disable SERVICE` — ihn als nicht eingetragen vermerken, sodass kein
  späterer Start ihn hochfährt, und ihn jetzt stoppen.

Bei Erfolg nennt eine Zeile den Zustand, in dem der Verwalter den Dienst
hinterlassen hat.

Beide Arten von Änderung betreffen jeden Prinzipal der Maschine, nicht nur
Ihre eigene Sitzung. `start` und `stop` ändern nur das *laufende* System, ein
eingetragener Dienst kommt also beim nächsten Start wieder; `enable` und
`disable` ändern den Eintrag selbst und überdauern ihn daher.

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
