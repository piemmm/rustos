## NAME

desktop — avviare la sessione grafica del desktop

## SYNOPSIS

`desktop`

## DESCRIPTION

Avvia la sessione grafica del desktop sulla postazione di questa
macchina: il comando acquisisce il lease esclusivo di schermo e input
della postazione, si connette al servizio di visualizzazione ed esegue
il desktop composito — il gestore delle finestre e la barra delle
applicazioni — finché la sessione non termina. Il comando ritorna
quando la sessione del desktop termina.

Lo stesso desktop parte automaticamente dopo l'autenticazione quando
l'amministratore ha configurato un accesso grafico
(`configure os.loginType graphical`); questo comando lo avvia su
richiesta da una shell testuale.

Quando nessun servizio di visualizzazione è in esecuzione, o un'altra
sessione detiene già la postazione, il comando fallisce scrivendo il
motivo sull'errore standard — non spodesta mai una sessione in corso.

## OPTIONS

- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `desktop` — avviare la sessione del desktop.

## EXIT STATUS

- `0` — la guida breve è stata servita.
- `2` — la riga di comando non è stata compresa.
- qualsiasi altro codice diverso da zero — la sessione non è potuta
  partire (nessuna postazione, nessun servizio di visualizzazione) o è
  terminata (il lease della postazione è andato perso); il motivo è
  scritto sull'errore standard.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

- `configure`
- `man`
