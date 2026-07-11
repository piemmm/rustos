## NAME

unmount — scollegare un volume montato

## SYNOPSIS

`unmount [option...] name`

## DESCRIPTION

Mette fuori servizio il volume montato sotto `name`: il filesystem e
il dispositivo vengono svuotati, il montaggio sotto `/Storage` viene
rimosso e la radice durevole `id::` del volume viene revocata. `name`
è il nome di catalogo del volume (`usb1`) o il suo punto di montaggio
(`/Storage/usb1`), confrontato con l'elenco dei montaggi dell'API di
informazioni di sistema.

Un volume il cui dispositivo è stato rimosso con scritture non ancora
confermate resta visibile come `unavailable-dirty` (o
`unavailable-lost`), e un `unmount` semplice rifiuta: i dati
trattenuti sono conservati per un reinserimento verificato. `--force`
è l'uscita deliberata — i dati trattenuti vengono scartati, il volume
viene rimosso e la perdita è registrata nel registro di controllo. Su
un volume sano `--force` svuota e scollega comunque in modo pulito;
nulla viene scartato quando una conferma pulita è possibile.

Lo scollegamento richiede l'autorità di montaggio (`CAP_FS_MOUNT`);
il kernel la verifica e registra ogni decisione. I volumi di avvio
permanenti e i collegamenti di vista del sistema non sono
scollegabili.

## OPTIONS

- `-f, --force` — smontaggio forzato: rimuovere il volume anche
  quando i suoi dati non possono essere confermati, scartando i dati
  trattenuti.
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `unmount usb1` — scollegare in modo pulito il volume montato come
  `usb1`.
- `unmount /Storage/usb1` — lo stesso, indicato dal punto di
  montaggio.
- `unmount --force usb1` — rimuovere un volume non disponibile
  scartando i suoi dati trattenuti.

## EXIT STATUS

- `0` — il volume è stato scollegato (o la guida breve è stata
  scritta).
- `1` — il volume non è stato trovato, non è scollegabile o il kernel
  ha rifiutato lo scollegamento.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve
  (un'etichetta BCP-47 come `it-IT`).

## SEE ALSO

- `mount`
- `df`
- `man`
