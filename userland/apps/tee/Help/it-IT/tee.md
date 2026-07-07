## NAME

tee — leggere dallo standard input e scrivere sullo standard output e in file

## SYNOPSIS

`tee [opzione...] [file...]`

## DESCRIPTION

Copia lo standard input sullo standard output e in ogni file indicato,
così i dati di una pipeline possono essere visti e catturati insieme.
Ogni file viene creato se assente e sovrascritto, a meno che `-a` non
accodi. Un file che non può essere aperto o scritto viene segnalato e
l'esecuzione continua con le uscite rimanenti, secondo la modalità
`--output-error` scelta.

RustOS non ha `SIGPIPE`: la scomparsa di un consumatore si manifesta
come un errore di scrittura sullo standard output — l'unica uscita di
questo comando che può essere una pipe — quindi la «pipe» delle
modalità GNU qui indica esattamente quell'uscita. Senza
`--output-error`, uno standard output guasto ferma l'esecuzione
(l'equivalente dello strumento GNU ucciso da `SIGPIPE`, con la ragione
indicata sullo standard error); con una modalità `-nopipe` viene
tollerato in silenzio.

GNU `tee -i` (ignorare le interruzioni) non è disponibile: RustOS non
ha una disposizione dei segnali per processo da impostare.
L'interruttore arriverà con quel lavoro nel kernel invece di essere
accettato e ignorato.

## OPTIONS

- `-a, --append` — accodare ai file indicati; non sovrascriverli.
- `-p` — tollerare in silenzio uno standard output guasto; lo stesso di
  `--output-error=warn-nopipe`.
- `--output-error[=<mode>]` — come trattare un'uscita guasta. Senza
  valore, `warn-nopipe`. Le modalità (è accettato un prefisso non
  ambiguo): `warn` — segnalare un errore di scrittura su qualsiasi
  uscita, scartare quell'uscita e continuare; `warn-nopipe` — come
  `warn`, ma uno standard output guasto viene scartato in silenzio e
  non cambia lo stato di uscita; `exit` — segnalare un errore di
  scrittura su qualsiasi uscita e fermarsi; `exit-nopipe` — come
  `exit`, ma uno standard output guasto viene scartato in silenzio.
- `-h, -?` — mostrare la guida breve di questo comando.
- `--` — terminare l'analisi delle opzioni; ogni argomento successivo
  indica un file, e un operando `-` indica un file chiamato `-`.

## EXAMPLES

- `ls -l | tee listing.txt` — mostrare l'elenco e salvarne una copia.
- `make 2>&1 | tee -a build.log` — accodare la trascrizione di una
  compilazione mentre la si osserva.
- `cat data | tee copy1 copy2 | wc -c` — catturare due copie e contare
  i byte che proseguono.

## EXIT STATUS

- `0` — ogni uscita è stata servita fino alla fine dell'input (o è
  stata mostrata la guida breve richiesta); un guasto dello standard
  output tollerato da una modalità `-nopipe` non lo cambia.
- `1` — un'uscita è fallita in un modo che la modalità scelta conta,
  oppure l'input non è stato leggibile.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `cat`
- `head`
- `wc`
