## NAME

basename — togliere directory e suffisso dai nomi

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

Stampa il componente finale di ogni scrittura di percorso: vengono
tolte le barre finali, poi tutto ciò che precede l'ultima barra
rimasta, essa inclusa. L'operazione è puramente lessicale — nessun
percorso viene risolto né toccato su disco. Con un `suffix` (il secondo
operando, o `-s`) viene tolto anche un `suffix` finale, a meno che non
costituisca l'intero nome rimasto.

Una radice non viene mai intaccata: `basename /` è `/`, e —
l'equivalente nella foresta di archiviazione RustOS — `basename Home:/`
è `Home:/`. Una radice di alias (`Home:/`, `System:/`, …) svolge
esattamente il ruolo che `/` svolge sui sistemi POSIX.

Senza `-a` né `-s` si accettano al più due operandi: il nome e un
suffisso facoltativo. Con `-a` (o `-s`, che lo implica) ogni operando è
un nome.

## OPTIONS

- `-a, --multiple` — trattare ogni operando come un nome.
- `-s, --suffix <suffix>` — togliere un `suffix` finale da ogni nome;
  implica `-a`. Si scrive anche `--suffix=<suffix>` o raggruppato
  (`-s.rs`).
- `-z, --zero` — terminare ogni risultato con NUL invece dell'a-capo.
- `-h, -?` — mostrare la guida breve di questo comando.

## EXAMPLES

- `basename /System/Apps/top.app` — stampare `top.app`.
- `basename src/lib.rs .rs` — stampare `lib`.
- `basename -s .rs -a a.rs b.rs` — stampare `a` e `b`.
- `basename Home:/` — stampare `Home:/` (una radice non viene mai
  intaccata).

## EXIT STATUS

- `0` — i risultati (o la guida breve) sono stati scritti.
- `1` — l'output non è stato consegnato.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `dirname`
- `man`
