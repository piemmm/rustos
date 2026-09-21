## NAME

usermod — modificare un account utente

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

Cambia i campi d'identità di un account, il suo stato di blocco o il suo
tetto di capacità. Modificare un account è un'operazione amministrativa: il
database rifiuta un chiamante privo della capacità di amministrazione degli
utenti.

Una modifica d'identità sostituisce l'intero insieme di campi non attinenti
alla sicurezza, perciò lo strumento legge prima il record corrente e rinvia
immutato ogni campo che nessuno ha nominato. Un account che il database non
elenca è rifiutato prima di inviare alcunché.

Ogni opzione è una propria operazione del database, applicata una per volta
e per intero o per nulla. Una riga che ne chiede diverse ne emette diverse,
in un ordine fisso — campi, poi capacità, poi blocco — e si ferma al primo
rifiuto, nominando il passo e avvertendo che una modifica precedente può già
essere in vigore.

`-G` sostituisce l'intero insieme supplementare invece di aggiungervisi: il
database prende insiemi interi, e un'aggiunta fondata su una lettura vecchia
sarebbe peggio di una sostituzione esplicita. `--grants` è un concetto
proprio di TAIRiX, scritto solo in forma lunga; il database rifiuta ogni
capacità che l'account chiamante non possiede.

`--` termina l'analisi delle opzioni: ogni argomento successivo è un operando.

## OPTIONS

- `-c, --comment COMMENT` — il commento / nome completo dell'account.
- `-d, --home HOME` — la directory personale.
- `-s, --shell SHELL` — la shell di accesso.
- `-g, --gid GID` — l'identificatore numerico del gruppo primario.
- `-G, --groups LIST` — gli identificatori numerici dei gruppi supplementari
  separati da virgole, che sostituiscono l'insieme corrente. Un elenco vuoto
  lo azzera.
- `-L, --lock` — impedire l'accesso all'account.
- `-U, --unlock` — consentirlo di nuovo.
- `--grants LIST` — i nomi di capacità separati da virgole che formano
  l'intero tetto. Un elenco vuoto lo azzera.
- `-h, -?, --help` — mostrare l'aiuto breve proprio di questo comando.

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — impostare il nome completo dell'account.
- `usermod -L ada` — bloccare l'account.
- `usermod --grants LIST` — sostituire il tetto di capacità.

## EXIT STATUS

- `0` — ogni modifica richiesta è stata fatta.
- `1` — il database ha rifiutato o non ha potuto completare una modifica; il
  passo e la ragione sono stampati sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47 come `it-IT`).

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
