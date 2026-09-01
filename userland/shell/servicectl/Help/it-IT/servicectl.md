## NAME

servicectl — avviare, fermare, abilitare e disabilitare i servizi di sistema

## SYNOPSIS

`servicectl [-h | -?] start|stop|enable|disable SERVICE`

## DESCRIPTION

Chiede al gestore dei servizi di cambiare lo stato di esecuzione di un
servizio registrato, tramite il suo endpoint di controllo protetto da
capacità. Decide il gestore: questo strumento codifica solo la richiesta e
riporta la risposta.

Raggiungere l'endpoint è di per sé l'autorizzazione. Senza
`CAP_SERVICE_CONTROL` nel massimale del vostro account il kernel rifiuta la
chiamata prima che il gestore la veda; un account non privilegiato non può
nemmeno chiedere.

- `start SERVICE` — avviare ora un servizio registrato attualmente fermo.
  Le condizioni di prontezza che richiede restano valide: un servizio le
  cui condizioni non sono soddisfatte viene rifiutato anziché avviato in un
  sistema che non può sostenerlo.
- `stop SERVICE` — fermare con grazia un servizio in esecuzione, e i suoi
  dipendenti in ordine inverso di dipendenza. Al servizio viene chiesto di
  terminare e viene forzato solo dopo il suo periodo di grazia.
- `enable SERVICE` — registrare il servizio come iscritto, così che il
  gestore lo avvii a ogni avvio, e avviarlo ora.
- `disable SERVICE` — registrarlo come non iscritto, così che nessun avvio
  successivo lo avvii, e fermarlo ora.

In caso di successo una riga nomina lo stato in cui il gestore ha lasciato
il servizio.

Entrambi i tipi di cambiamento riguardano ogni principale della macchina,
non solo la vostra sessione. `start` e `stop` cambiano solo il sistema *in
esecuzione*, quindi un servizio iscritto ritorna al prossimo avvio; `enable`
e `disable` cambiano l'iscrizione stessa e perciò gli sopravvivono.

## OPTIONS

- `-h, -?` — mostrare l'aiuto breve di questo comando ed uscire.
- `--` — terminare le opzioni, così che un servizio il cui nome inizia con
  un trattino possa comunque essere nominato.

## EXIT STATUS

- `0` — l'operazione è stata applicata, o è stato mostrato l'aiuto breve.
- `1` — il gestore ha rifiutato l'operazione, o l'endpoint di controllo non
  è stato raggiungibile.
- `2` — la riga di comando non è stata compresa; non è stato inviato nulla.

## ENVIRONMENT

- `LANG` — la locale preferita per l'aiuto breve (un tag BCP-47 come `fr-FR`).

## SEE ALSO

- `ps`
- `man`
