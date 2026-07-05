## NAME

ls — elencare il contenuto delle directory

## SYNOPSIS

`ls [-aAdFghlmnopQrRS1] [--] [path...]`

## DESCRIPTION

Elenca ogni operando di percorso: le voci di un operando directory
vengono lette ed elencate (a meno che `-d` non designi la directory
stessa), qualsiasi altro operando è elencato così com'è. Senza
operandi viene elencata la directory corrente (`.`).

Le voci sono ordinate per nome (o per dimensione, la più grande per
prima, con `-S`; invertite con `-r`), un nome per riga per
impostazione predefinita. Le voci il cui nome inizia con `.` sono
nascoste salvo che sia dato `-a` o `-A`; quando delle voci vengono
nascoste, una nota è emessa sul flusso informativo standard (fd 3),
mai nell'elenco stesso.

Il formato lungo (`-l`) mostra i bit di tipo e permessi, il
proprietario e il gruppo, la dimensione e poi il nome. Proprietario e
gruppo sono identificatori numerici: risolvere i nomi degli account
richiede il database utenti protetto da capacità, che un elenco non
deve pretendere; l'output corrisponde quindi al ripiego numerico
dello strumento GNU (`-n` produce lo stesso). Non ci sono colonne per
il numero di collegamenti né per le marche temporali perché il
contratto del file system non porta ancora collegamenti fisici né
marche temporali; le colonne compariranno quando lo farà.

Quando sono dati più operandi — e sempre sotto `-R` — l'elenco di
ogni directory è preceduto da un'intestazione `percorso:`, e i
blocchi sono separati da una riga vuota.

## OPTIONS

- `-a, --all` — non nascondere le voci il cui nome inizia con `.`.
- `-A, --almost-all` — come `-a`, ma senza mai elencare `.` o `..`.
- `-d, --directory` — elencare gli operandi directory stessi, non il
  loro contenuto.
- `-F, --classify` — aggiungere `/` alle directory e `*` agli
  eseguibili.
- `-g` — formato lungo senza la colonna del proprietario; implica
  `-l`.
- `-h, --human-readable` — con `-l`, mostrare le dimensioni come
  `1.1K`, `23M` (potenze di 1024).
- `-l` — formato lungo: bit dei permessi, proprietario, gruppo,
  dimensione e poi nome.
- `-m` — nomi separati da virgole su una riga.
- `-n, --numeric-uid-gid` — formato lungo con proprietario e gruppo
  numerici; implica `-l`. Proprietario e gruppo sono sempre numerici
  qui (vedi sopra), quindi coincide con `-l`.
- `-o` — formato lungo senza la colonna del gruppo; implica `-l`.
- `-p` — aggiungere `/` alle directory.
- `-Q, --quote-name` — racchiudere ogni nome tra virgolette doppie,
  con escape di virgolette, barre rovesciate e caratteri di
  controllo.
- `-r, --reverse` — invertire l'ordine di ordinamento.
- `-R, --recursive` — elencare le sottodirectory ricorsivamente.
- `-S` — ordinare per dimensione, la più grande per prima.
- `-1` — un nome per riga (l'impostazione predefinita).
- `-?` — mostrare la guida breve di questo comando (`--help` è la
  forma lunga).

## EXAMPLES

- `ls` — elencare la directory corrente.
- `ls -al /System` — elenco in formato lungo di `/System`, voci
  nascoste comprese.
- `ls -lhS` — formato lungo, dimensioni leggibili, la più grande per
  prima.
- `ls -R Documents` — attraversare `Documents` ricorsivamente,
  un'intestazione per directory.
- `ls -F` — contrassegnare le directory con `/` e gli eseguibili con
  `*`.
- `ls -d Documents` — elencare la voce `Documents` stessa, non il suo
  contenuto.

## EXIT STATUS

- `0` — ogni operando è stato elencato.
- `1` — un operando non è stato ispezionato o una directory non è
  stata letta, oppure l'output non è stato consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `cat`
- `man`
