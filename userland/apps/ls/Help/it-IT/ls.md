## NAME

ls — elencare il contenuto delle directory

## SYNOPSIS

`ls [-a] [-l] [--] [path...]`

## DESCRIPTION

Elenca ogni operando di percorso in ordine. Per una directory ne
vengono elencate le voci, ordinate per nome; un operando che non è una
directory è elencato con il suo nome. Senza operandi viene elencata la
directory corrente.

Le voci il cui nome inizia con `.` sono nascoste salvo che sia indicato
`-a`. Quando il filtro predefinito nasconde delle voci, `ls` ne annota
il numero sul flusso consultivo (fd 3); l'elenco in sé non cambia.

Con più di un operando vengono elencati prima quelli che non sono
directory (ordinati per nome), poi ogni directory sotto un'intestazione
`percorso:`, con i blocchi separati da una riga vuota.

Il formato lungo stampa, per ogni voce: un carattere di tipo (`d` per
una directory, `-` altrimenti), i nove bit dei permessi, la dimensione
in byte allineata a destra nel blocco e infine il nome.

## OPTIONS

- `-a, --all` — non nascondere le voci il cui nome inizia con `.`.
- `-l, --long` — formato lungo: tipo e bit dei permessi, dimensione,
  poi nome.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `ls` — elencare la directory corrente.
- `ls -la /System/Apps` — elencare ogni voce di `/System/Apps`,
  comprese quelle nascoste, nel formato lungo.
- `ls -- -a` — elencare il file o la directory di nome `-a`.

## EXIT STATUS

- `0` — ogni operando è stato elencato.
- `1` — non è stato possibile ispezionare un operando, leggere una
  directory o consegnare l'elenco.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un tag
  BCP-47 come `it-IT`).

## SEE ALSO

- `man`
