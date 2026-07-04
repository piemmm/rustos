## NAME

ps — elencare i processi

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

Elenca i processi tramite l'API di informazioni di sistema. Per
impostazione predefinita sono elencati solo i processi del chiamante;
il servizio applica ogni ambito di interrogazione secondo l'identità
del chiamante attestata dal kernel, e nessun percorso elude quel
controllo.

Ogni processo è stampato come una riga sotto un'intestazione di
colonne: l'identificatore del processo (`PID`), quello del processo
padre (`PPID`), gli identificatori di utente e gruppo proprietari
(`UID`, `GID`), lo stato di schedulazione (`S`), la CPU su cui il
processo è stato eseguito per ultimo (`CPU`), e il nome del comando
(`NAME`).

`ps` non accetta operandi.

## OPTIONS

- `-e, -A, --all` — elencare tutti i processi del sistema anziché solo
  quelli del chiamante; il servizio concede questa vista solo a un
  chiamante che detiene `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `ps` — elencare i propri processi.
- `ps -e` — elencare tutti i processi del sistema.

## EXIT STATUS

- `0` — l'elenco è stato scritto.
- `1` — il servizio ha rifiutato o è fallito, oppure l'elenco non è
  stato consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

- `man`
- `top`
- `sysinfo`
