## NAME

passwd — impostare la password di un account

## SYNOPSIS

`passwd [--record RECORD] [--] NAME`

## DESCRIPTION

Sostituisce la password memorizzata dell'account nominato. Impostare una
password è un'operazione amministrativa: il database rifiuta un chiamante
privo della capacità di amministrazione degli utenti.

Nessuna password in chiaro attraversa la chiamata di sistema. Lo strumento
chiede due volte con l'eco del terminale spento, riduce quanto digitato a un
record PBKDF2 con sale — tratto dalla sorgente casuale del nucleo — e invia
il record; entrambi i buffer in chiaro sono azzerati appena esso esiste.

Il nome dell'account è obbligatorio. Il `passwd` di GNU senza operando
cambia la password del chiamante stesso, il che su TAIRiX richiederebbe una
via di autoservizio non privilegiata che non esiste: l'intera interfaccia di
amministrazione è protetta da capacità, e ritagliarvi «il tuo record»
sarebbe un cambio del modello di sicurezza, non una comodità.

Un chiamante senza terminale — un programma grafico, il cui ingresso
standard è chiuso sotto l'intermediario di elevazione — calcola da sé il
record e consegna quello finito con `--record`, così nessun chiaro esiste da
nessuna delle due parti. Il record è verificato come ben formato prima di
essere memorizzato.

`--` termina l'analisi delle opzioni: ogni argomento successivo è un operando.

## OPTIONS

- `--record RECORD` — un record PBKDF2 con sale già pronto, per un chiamante
  senza terminale a cui chiedere.
- `-h, -?, --help` — mostrare l'aiuto breve proprio di questo comando.

## EXAMPLES

- `passwd ada` — chiedere due volte e impostare la password dell'account.

## EXIT STATUS

- `0` — la password è stata sostituita.
- `1` — il database ha rifiutato o non ha potuto completare la sostituzione,
  le due digitazioni differivano, non c'era casualità disponibile, o il
  record era malformato; la ragione è stampata sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un'etichetta BCP-47 come `it-IT`).

## SEE ALSO

- `useradd`
- `usermod`
- `userdel`
- `users`
