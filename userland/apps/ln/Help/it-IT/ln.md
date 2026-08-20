## NAME

ln — creare collegamenti simbolici

## SYNOPSIS

`ln -s [-finvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Crea un collegamento simbolico che nomina ciascuna destinazione. Con un
solo operando il collegamento è creato nella directory di lavoro col
nome proprio della destinazione. Con due, il secondo operando è una
directory da riempire se lo è — o un collegamento a una, tranne con
`-n` — e il nome del collegamento altrimenti. Con tre o più, l'ultimo
deve già essere una directory.

La destinazione è memorizzata **letteralmente** e non è mai risolta: può
essere relativa, contenere `..` e non nominare nulla, quindi un
collegamento può legittimamente pendere. La sua grammatica è comunque
verificata prima della scrittura, così una destinazione che nessun
risolutore potrebbe percorrere è rifiutata. Creare un collegamento non
concede alcuna autorità su ciò che nomina: ogni uso successivo è
autorizzato componente per componente sotto la vostra identità.

Un nome di collegamento già occupato è rifiutato a meno che `-f` o `-i`
dica di sostituirlo, e la sostituzione **rimuove** prima quel nome, così
nulla passa attraverso un collegamento già presente verso ciò che indica.
Una directory non è mai sostituita.

Il primo errore ferma l'esecuzione prima di ogni destinazione
successiva; i collegamenti già creati restano. `--` termina l'analisi
delle opzioni: ogni argomento successivo è un operando.

`-s` è obbligatorio su questo sistema, che non ha collegamenti fisici:
senza di esso `ln` non ha nulla da creare, e lo dichiara invece di
creare un collegamento simbolico, che è un oggetto diverso. Le opzioni
riservate ai collegamenti fisici `-L`, `-P`, `-d` e `-F` sono rifiutate
per la stessa ragione. `-b`/`-S` sono rifiutate perché non esiste alcun
meccanismo di copia di sicurezza, e `-r` perché calcolare una
destinazione relativa alla directory del collegamento richiede una
risoluzione canonizzante che questo sistema non offre — una lessicale
nominerebbe un altro oggetto appena vi fosse un collegamento.

## OPTIONS

- `-s, --symbolic` — creare collegamenti simbolici. Obbligatorio: vedi
  sopra.
- `-f, --force` — rimuovere un nome di collegamento esistente e creare
  poi il collegamento.
- `-i, --interactive` — chiedere prima di rimuovere un nome di
  collegamento esistente; solo una risposta che inizia con `y`/`Y`
  acconsente. Vince l'ultima fra `-f` e `-i`.
- `-n, --no-dereference` — trattare una destinazione che è un
  collegamento simbolico a una directory come il semplice nome che è
  anche, invece che come directory in cui creare i collegamenti.
- `-v, --verbose` — segnalare ogni collegamento creato come
  `'link' -> 'target'`.
- `-t dir, --target-directory=dir` — creare ogni collegamento in `dir`,
  che deve già essere una directory. Il valore segue attaccato
  (`-tdir`, `--target-directory=dir`) o come argomento successivo.
- `-T, --no-target-directory` — trattare la destinazione come nome di
  collegamento, mai come directory da riempire; esattamente due
  operandi. Non combinabile con `-t`.
- `-h, -?, --help` — mostrare l'aiuto breve di questo comando.

## EXAMPLES

- `ln -s /System/Commands/ls.app tools/ls` — collegare un nome a un
  pacchetto.
- `ln -s ../shared/notes.txt` — collegare `notes.txt` qui a una
  destinazione relativa.
- `ln -sv -t Links a.txt b.txt` — collegare entrambi i file in `Links`
  segnalando ogni collegamento.
- `ln -sfn /Storage/media Music` — reindirizzare un collegamento
  `Music` esistente a una nuova directory, sostituendo il collegamento
  invece di collegare al suo interno.

## EXIT STATUS

- `0` — ogni collegamento è stato creato (o è stato scritto l'aiuto
  breve); una domanda `-i` rifiutata non è un errore.
- `1` — ogni altro caso, con il motivo sull'uscita di errore. Anche una
  riga di comando non compresa termina con `1`.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

- `ls`
- `cp`
- `rm`
