## NAME

flock — einen Befehl ausführen und dabei eine empfehlende Dateisperre halten

## SYNOPSIS

`flock [options] datei befehl [argument...]`

## DESCRIPTION

Nimmt eine empfehlende Sperre über die gesamte `datei`, führt den `befehl`
aus, während sie gehalten wird, und endet mit dem Status des Befehls. Zwei
Läufe, die dieselbe `datei` nennen, überschneiden sich damit nie — genau
das braucht ein Skript, damit immer nur eine Kopie von sich selbst läuft.

Die Sperre gehört zur offenen Datei dieses Laufs, also gibt das System sie
frei, wenn der Lauf endet: regulär, unterbrochen oder abgestürzt. In die
`datei` wird nichts geschrieben und danach ist nichts aufzuräumen, es
bleibt also keine veraltete Sperre übrig, auf die der nächste Lauf warten
müsste. Die `datei` wird angelegt, falls sie fehlt.

Sperren sind **empfehlend**: sie koordinieren Programme, die sich darauf
einlassen. Sie gewähren und verweigern keinen Zugriff, ein Programm ohne
Sperre wird also nicht am Lesen oder Schreiben gehindert. Wer lesen oder
schreiben darf, entscheiden Eigentümer, Rechte und Zugriffsliste der Datei
wie bei jeder anderen.

Ohne `-n` und `-w` wartet der Lauf, so lange es dauert. Mit `-n` gibt er
sofort auf, mit `-w` nach der angegebenen Zeit. In beiden Fällen endet das
Aufgeben mit dem Konfliktcode (`1`, sofern `-E` nichts anderes sagt), und
der Befehl läuft **nicht** — ein Skript kann „ich habe die Sperre nicht
bekommen“ also immer von „der Befehl ist fehlgeschlagen“ unterscheiden.

Drei Optionen der `flock`-Befehle anderer Systeme fehlen absichtlich,
anstatt angenommen und ignoriert zu werden. `-u` und `-o` wirken auf einen
Dateideskriptor, den eine Shell vorher geöffnet hat — diese Form gibt es
hier nicht; `-c` gibt sein Argument an eine Shell, was hier ausdrücklich
`flock datei elsh -c '...'` heißt, damit klar ist, welche Shell läuft. Eine
davon zu verlangen ist ein Anwendungsfehler, das Skript wird also gewarnt
statt ohne die verlangte Sperre zu laufen.

## OPTIONS

- `-s, --shared` — eine gemeinsame Sperre nehmen. Mehrere Läufe dürfen sie gleichzeitig halten, und alle schließen einen Lauf aus, der eine exklusive verlangt. Die Sperre für Leser.
- `-x, --exclusive` — eine exklusive Sperre nehmen, die jeden anderen Halter ausschließt. Die Voreinstellung und die Sperre für Schreiber.
- `-n, --nonblock` — nicht warten: ist die Sperre gehalten, sofort mit dem Konfliktcode enden.
- `-w, --timeout <seconds>` — höchstens so viele ganze Sekunden warten, dann mit dem Konfliktcode enden.
- `-E, --conflict-code <n>` — der Status beim Aufgeben nach `-n` oder `-w`. Voreingestellt `1`. Wählen Sie einen Wert, den der Befehl selbst nie liefert, wenn ein Skript beides unterscheiden muss.
- `-v, --verbose` — auf der Standardfehlerausgabe melden, ob die Sperre genommen wurde.
- `-?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `flock /Users/ian/Library/backup.lock backup-now` — die Sicherung
  starten und warten, falls schon eine Kopie läuft.
- `flock -n /Storage/db/data.lock compact` — die Datenbank verdichten oder
  sofort mit `1` enden, wenn die Sperre gehalten wird.
- `flock -s -w 30 /Storage/db/data.lock report` — eine Lesersperre nehmen
  und bis zu dreißig Sekunden auf einen Schreiber warten.
- `flock -E 99 -n lock task` — mit `99` statt `1` enden, wenn die Sperre
  gehalten wird, damit das Skript diesen Fall von einem Fehler in `task`
  unterscheiden kann.

## EXIT STATUS

- der Status des Befehls — die Sperre wurde genommen und der Befehl lief.
- der Konfliktcode (`1` als Voreinstellung) — `-n` oder `-w` hat
  aufgegeben; der Befehl lief nicht.
- `1` — die Sperre war aus einem Grund nicht zu bekommen, den Warten nicht
  behebt, oder der Befehl war nicht startbar; der Grund steht auf der
  Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden; es wurde nichts gesperrt
  und nichts ausgeführt.
- `126` — der Befehl wurde gefunden, war aber nicht ausführbar.
- `127` — der Befehl wurde nicht gefunden.

## ENVIRONMENT

- `PATH` — nach den Programmspeichern von System und Benutzer nach dem
  Befehl durchsucht.
- `HOME` — findet die eigenen Programmspeicher des Benutzers.
- `LANG` — die bevorzugte Locale der Kurzhilfe (ein BCP-47-Kennzeichen wie
  `fr-FR`).

## SEE ALSO

elsh, ps, ulimit
