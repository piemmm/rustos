## NAME

yes — scrivere ripetutamente una riga di testo

## SYNOPSIS

`yes [stringa...]`

## DESCRIPTION

Scrive i suoi operandi, uniti da spazi singoli — o `y` quando non ne
viene dato alcuno —, seguiti da un a-capo, più e più volte, finché
l'output smette di accettare byte (una pipe chiusa) o il processo viene
terminato. Il suo compito storico è fornire una risposta affermativa a
un comando che pone domande; quello moderno, essere una fonte economica
di testo ripetuto.

L'analisi delle opzioni si ferma al primo operando: `yes a -x` scrive
`a -x`. Un'opzione sconosciuta prima degli operandi è un errore;
scrivere `yes -- -x` per stampare una stringa che sembra un'opzione.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando.
- `--` — terminare l'analisi delle opzioni; ogni argomento successivo è
  un operando.

## EXAMPLES

- `yes` — scrivere `y` fino all'interruzione.
- `yes hello world` — scrivere `hello world` fino all'interruzione.
- `yes -- -x` — scrivere `-x` (dopo `--` gli operandi possono sembrare
  opzioni).

## EXIT STATUS

- `0` — la guida breve richiesta è stata servita.
- `1` — l'output ha smesso di accettare byte (l'unica condizione di
  arresto dello strumento).
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `true`
- `man`
