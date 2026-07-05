## NAME

groupadd — creare un gruppo

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

Aggiunge un singolo gruppo al registro dei gruppi. Il nome del gruppo
deve corrispondere a `[a-z_][a-z0-9_-]*` e l'identificatore è un valore
decimale. Creare un gruppo è un'operazione amministrativa: il registro
rifiuta un chiamante privo della capacità di amministrazione degli
utenti.

Quando `-g` è omesso, l'identificatore del gruppo è assegnato
automaticamente, uno sopra il più alto esistente. Un identificatore
richiesto già occupato viene rifiutato; il registro è l'autorità sulle
collisioni.

`--` termina l'analisi delle opzioni: ogni argomento successivo è un
operando.

## OPTIONS

- `-g, --gid GID` — identificatore numerico del gruppo; assegnato
  automaticamente quando è omesso (uno sopra il più alto esistente).
- `-h, -?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `groupadd staff` — creare `staff` con un identificatore assegnato
  automaticamente.
- `groupadd -g 100 staff` — creare `staff` con l'identificatore `100`.

## EXIT STATUS

- `0` — il gruppo è stato creato.
- `1` — il registro ha rifiutato la creazione o questa è fallita (per
  esempio una capacità mancante o un identificatore duplicato); il
  motivo è stampato sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `useradd`
- `users`
