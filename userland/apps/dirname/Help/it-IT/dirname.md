## NAME

dirname — togliere l'ultimo componente dai nomi

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

Stampa ogni scrittura di percorso senza il suo ultimo componente:
vengono tolte le barre finali, poi l'ultimo componente e le barre che
lo precedono. L'operazione è puramente lessicale — nessun percorso
viene risolto né toccato su disco. Una scrittura senza barre rimaste ha
come genitore `.`; un genitore che si svuota è la radice.

Una radice non viene mai intaccata: `dirname /tools` è `/`, e —
l'equivalente nella foresta di archiviazione RustOS —
`dirname Home:/tools` è `Home:/`. Una radice di alias (`Home:/`,
`System:/`, …) svolge esattamente il ruolo che `/` svolge sui sistemi
POSIX.

## OPTIONS

- `-z, --zero` — terminare ogni risultato con NUL invece dell'a-capo.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `dirname /System/Apps/top.app` — stampare `/System/Apps`.
- `dirname src/lib.rs` — stampare `src`.
- `dirname file` — stampare `.` (nessuna parte directory).
- `dirname Home:/tools` — stampare `Home:/` (una radice non viene mai
  intaccata).

## EXIT STATUS

- `0` — i risultati (o la guida breve) sono stati scritti.
- `1` — l'output non è stato consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `basename`
- `man`
