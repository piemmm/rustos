## NAME

elsh — la shell dei comandi di TAIRiX

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Esegue una shell dei comandi interattiva — un ciclo
leggi-valuta-stampa sui flussi standard ereditati. Una parola di
comando digitata è risolta prima fra i comandi integrati della shell,
poi nello store di comandi di sistema (`/System/Commands`), nello store
di applicazioni di sistema (`/System/Applications`), nello store di
comandi (`<home>/Commands`) e nello store di applicazioni
(`<home>/Applications`) dell'utente stesso, poi nelle directory della
variabile `PATH`; questi quattro store formano un prefisso fisso che
l'utente non può riordinare né sovrascrivere, quindi `PATH` non può mai
oscurare un comando di sistema. Una parola non risolta esce con `127`;
un bundle risolto ma non eseguibile esce con `126`.

I comandi integrati:

- `cd <path>`, `pwd` — cambiare e stampare la directory di lavoro.
- `echo ...` — stampare i propri operandi.
- `export NAME=value`, `unset NAME` — modificare l'ambiente esportato.
- `jobs`, `fg`, `bg` — controllo dei job.
- `ulimit` — leggere e imporre limiti di risorse.
- `elevate` — eseguire un comando ri-autenticato tramite il supervisore
  di accesso della console.
- `help` — elencare i comandi integrati.
- `exit [code]` — terminare la sessione.

La shell non accetta operandi: l'esecuzione di script non fa ancora
parte della sua grammatica.

Su un terminale la shell offre un editor di riga interattivo: Su/Giù
scorrono la cronologia dei comandi, `Ctrl-R` la cerca, `Ctrl-C` scarta
la riga in corso, `Ctrl-D` su una riga vuota termina la sessione e Tab
completa nomi di comandi, percorsi e riferimenti a risorse come
`sys:random`.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando e uscire.

## EXIT STATUS

- Il codice del comando integrato `exit`, oppure `0` quando il flusso
  d'ingresso termina (o è stata mostrata la guida breve).
- `2` — l'invocazione non è stata compresa.

## ENVIRONMENT

- `PATH` — le directory cercate dopo il prefisso fisso di store.
- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`), esportata a ogni comando lanciato.

## SEE ALSO

- `man`
