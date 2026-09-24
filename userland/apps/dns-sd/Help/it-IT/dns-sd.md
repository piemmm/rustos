## NAME

dns-sd — esplorare, risolvere e cercare i servizi del collegamento locale

## SYNOPSIS

`dns-sd [-t seconds] -B [type [domain]]`

`dns-sd [-t seconds] -L instance type [domain]`

`dns-sd [-t seconds] -G v4|v6|v4v6 host`

## DESCRIPTION

Interroga la rete locale — il collegamento — sui servizi che offre, tramite il
servizio di rilevamento del collegamento locale del sistema. `dns-sd` non
parla mai da sé il DNS multicast: il servizio di rilevamento interroga il
segmento per suo conto e ammette ogni richiesta secondo l'autorità propria del
programma.

Con `-B` esplora le istanze di un tipo di servizio, come `_ipp._tcp` per le
stampanti, e mostra ogni istanza quando viene aggiunta o rimossa. Senza tipo,
elenca ogni tipo di servizio che il collegamento offre. Con `-L` risolve
un'istanza nell'host e nella porta in cui si raggiunge e mostra ciò che
l'istanza dice di sé (i suoi attributi `TXT`). Con `-G` cerca gli indirizzi di
un host sotto `local`.

Ogni risposta indica l'interfaccia su cui è stata appresa, e una riga `Flush`
significa che tutto ciò che è stato appreso su quell'interfaccia non è più
noto — il suo collegamento è caduto, o il servizio è ripartito. Ogni nome
mostrato è stato scelto da un'altra macchina del collegamento, quindi è
mostrato con escape, nella forma di presentazione DNS: uno spazio come
`\032`, un carattere di controllo con il suo codice decimale.

Esplorare ogni tipo, o un tipo non concesso all'account in uso, richiede
`CAP_NET_DISCOVER_ALL`, che solo un account amministratore possiede. L'unico
dominio del collegamento è `local`.

## OPTIONS

- `-B` — esplorare le istanze di un tipo di servizio, o ogni tipo se non ne è
  indicato alcuno.
- `-L` — risolvere un'istanza di un tipo di servizio.
- `-G` — cercare gli indirizzi IPv4 (`v4`), IPv6 (`v6`) o entrambi (`v4v6`)
  di un host.
- `-t` — fermarsi dopo tanti secondi invece di funzionare fino
  all'interruzione.
- `-?, --help` — mostrare l'aiuto breve di questo comando.

## EXAMPLES

- `dns-sd -B _ipp._tcp` — le stampanti del collegamento, man mano che vanno e
  vengono.
- `dns-sd -t 5 -B` — ogni tipo di servizio visto entro cinque secondi.
- `dns-sd -L "Hall Printer" _ipp._tcp` — dove si raggiunge una stampante.
- `dns-sd -G v4v6 printer.local` — gli indirizzi di un host.

## EXIT STATUS

- `0` — il comando è arrivato in fondo (o l'aiuto breve è stato scritto).
- `1` — il servizio di rilevamento ha rifiutato la richiesta o non è in
  esecuzione.
- `2` — la riga di comando non è stata compresa, o l'output non è stato
  scritto.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per l'aiuto breve (un'etichetta
  BCP-47 come `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `man`
