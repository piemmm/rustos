## NAME

useradd — creare un account utente

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

Aggiunge un singolo account al database degli utenti. Il nome di accesso
deve corrispondere a `[a-z_][a-z0-9_-]*`; il gruppo primario (`-g`) è
obbligatorio e ogni riferimento a un gruppo o a un utente è un
identificatore decimale. Creare un account è un'operazione
amministrativa: il database rifiuta un chiamante privo della capacità di
amministrazione degli utenti.

L'account creato non ha **alcuna password utilizzabile**: nessuna
password vi corrisponde finché un amministratore non ne imposta una (e
nessuna può essere indovinata), esattamente come lo strumento GNU crea un
account disabilitato. Impostare poi una password con il comando `passwd`
dello strumento `users`.

Quando `-u` è omesso, l'identificatore è assegnato automaticamente, uno
sopra il più alto esistente. Quando `-d` è omesso, la directory
personale segue la disposizione standard `/Users/NAME`. L'account avvia
la shell predefinita del sistema e il tetto di capacità di sessione
ordinario; un amministratore lo amplia poi con il comando `grant` dello
strumento `users`.

`--` termina l'analisi delle opzioni: ogni argomento successivo è un
operando.

## OPTIONS

- `-u, --uid UID` — identificatore numerico dell'utente; assegnato
  automaticamente quando è omesso (uno sopra il più alto esistente).
- `-g, --gid GID` — identificatore numerico del gruppo primario.
  Obbligatorio: non c'è una politica di gruppo predefinito da
  indovinare.
- `-G, --groups LIST` — identificatori numerici dei gruppi
  supplementari, separati da virgole.
- `-c, --comment TEXT` — commento dell'account / nome completo
  visualizzato.
- `-d, --home PATH` — directory personale; `/Users/NAME` quando è
  omessa.
- `-h, -?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `useradd -g 100 alice` — creare `alice` nel gruppo primario `100` con
  un identificatore assegnato automaticamente.
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — ogni campo
  indicato.

## EXIT STATUS

- `0` — l'account è stato creato.
- `1` — il database ha rifiutato la creazione o questa è fallita (per
  esempio una capacità mancante, un identificatore duplicato o un gruppo
  sconosciuto); il motivo è stampato sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `groupadd`
- `users`
