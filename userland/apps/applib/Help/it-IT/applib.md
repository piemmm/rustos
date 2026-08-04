## NAME

applib — amministra la libreria dei programmi del desktop

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

Amministra la libreria dei programmi — il catalogo organizzato in
cartelle delle applicazioni avviabili presentato dal launcher del
desktop. La libreria è costituita da dati sul volume, mai una lista
compilata internamente: un archivio a livello di macchina in
`/System/Settings/ProgramLibrary/library.conf` che ogni account legge,
più un overlay opzionale per utente allo stesso percorso all'interno
dei `Settings/` dell'utente stesso. Ciò che un launcher mostra è il
risultato della risoluzione di entrambi: le voci e le regolazioni
dell'utente prevalgono su quelle a livello di macchina.

Senza sottocomandi (o con `list`), la libreria risolta viene stampata
cartella per cartella, una voce per riga: identificatore, nome
visualizzato e percorso del pacchetto — esattamente ciò che il
launcher mostra. Le cartelle sono l'insieme chiuso `Accessories`,
`Graphics`, `Internet`, `Multimedia`, `Office`, `Programming`, `Games`,
`SystemTools`, `Utilities` e `Other`; non esistono cartelle a formato
libero.

`applib add` registra un pacchetto applicativo. La sua identità, il
nome visualizzato, la cartella e l'icona sono tratti dal manifesto
`AppInfo` firmato del pacchetto stesso; `--category`, `--name` e
`--icon` sovrascrivono il manifesto. Un pacchetto il cui manifesto non
dichiara alcuna cartella di libreria necessita di una `--category`
esplicita — lo strumento non tenta mai di indovinare. `applib remove`
rimuove un record, identificato dal suo identificatore o dal percorso
del pacchetto con cui è stato registrato.

`applib hide` sopprime una voce dalla libreria risolta senza
rimuovere il suo record — il suo identificatore rimane prenotato, in
modo che un successivo `rescan` non possa resuscitarlo — e
`applib show` lo mostra nuovamente. Nascondere è una questione di
presentazione, mai di autorità: l'avvio di un pacchetto è sempre
governato dai controlli di firma e capacità del loader,
indipendentemente dal catalogo.

`applib rescan` esamina gli archivi delle applicazioni
(`/System/Commands`, `/System/Applications` e `/Apps`, o i
`<home>/Commands` e `<home>/Applications` del chiamante con `--user`),
legge il manifesto di ogni pacchetto e registra ogni applicazione che
richiede di essere elencata e non è ancora catalogata. I record
esistenti — compresi i rinomini e le soppressioni di un curatore — non
vengono mai disturbati, e un pacchetto con un manifesto illeggibile o
malformato viene saltato e conteggiato, senza mai causare
l'interruzione. È così che la libreria di un sistema nuovo si popola dai
pacchetti effettivamente installati, senza alcuna lista mantenuta
manualmente.

Per impostazione predefinita, lo strumento modifica l'archivio a
livello di macchina, che solo un principale ammesso dalla politica di
scrittura di `/System/Settings` può cambiare; un account ordinario lo
legge ma lo personalizza attraverso il proprio overlay con `--user`.
Una scrittura negata riporta la sua ragione e non cambia nulla.

In caso di successo, lo strumento non produce output sullo standard
output; l'esito di una modifica viene emesso come un record
informativo strutturato sullo standard information stream (fd 3), che
gli script possono catturare con `3>records.jsonl` e tutto il resto
può ignorare.

## OPTIONS

- `--category <folder>` — con `list`, mostra solo quella cartella; con
  `add`, inserisce la voce sotto di essa (sovrascrivendo la
  dichiarazione del manifesto).
- `--name <name>` — con `add`, il nome da visualizzare invece di
  quello del manifesto.
- `--icon <asset>` — con `add`, l'asset dell'icona (un nome file
  all'interno della cartella `Resources/` del pacchetto) invece di
  quella del manifesto.
- `--user` — applica la modifica all'overlay del chiamante (o, con
  `rescan`, esamina i `<home>/Commands` e `<home>/Applications` del
  chiamante) invece che all'archivio a livello di macchina.
- `-h, -?` — mostra l'aiuto breve di questo comando.

## EXAMPLES

- `applib` — mostra la libreria risolta, cartella per cartella.
- `applib list --category Games` — mostra una singola cartella.
- `applib add /Apps/chess.app` — registra un pacchetto come richiesto
  dal suo manifesto.
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  registra un pacchetto che non dichiara alcuna elencazione, sotto una
  cartella esplicita.
- `applib remove os.tairix.chess` — rimuove una voce per
  identificatore.
- `applib hide os.tairix.chess --user` — lo nasconde solo dalla
  propria libreria.
- `applib rescan` — registra ogni pacchetto installato ed elencato non
  ancora presente nel catalogo della macchina.

## EXIT STATUS

- `0` — l'elenco, la modifica, il rescan o l'aiuto breve sono stati
  completati.
- `1` — un errore di archivio, pacchetto o output (ad esempio, il
  chiamante non può modificare il catalogo a livello di macchina); la
  ragione è indicata sullo stream diagnostico.
- `2` — la riga di comando non è stata compresa, la cartella o la voce
  è sconosciuta, o il pacchetto non può essere registrato come
  richiesto.

## ENVIRONMENT

- `LANG` — il locale preferito per l'aiuto breve (un tag BCP-47 come
  `fr-FR`).
- `HOME` — la directory home del chiamante: identifica l'overlay per
  utente e le radici del rescan `--user` `<home>/Commands` e
  `<home>/Applications`.

## SEE ALSO

- `man`
- `configure`
