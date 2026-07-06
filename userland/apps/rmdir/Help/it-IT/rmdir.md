## NAME

rmdir — rimuovere directory vuote

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...`

## DESCRIPTION

Rimuove ogni directory indicata come operando, in ordine. Viene rimossa
solo una **directory vuota**: il filesystem stesso rifiuta un file (o
qualsiasi altro oggetto) e una directory con contenuto, in modo
atomico, cosicché nient'altro può mai essere rimosso al suo posto. Per
i file si usa `rm`, per gli alberi con contenuto `rm -r`.

Con `-p` vengono rimossi anche gli antenati di ogni operando, dal più
interno al più esterno: `rmdir -p a/b/c` rimuove `a/b/c`, poi `a/b`,
poi `a`. La radice nuda di un percorso (`/` o una radice alias come
`Home:/`) non viene mai richiesta.

Con `--ignore-fail-on-non-empty` un rifiuto «directory non vuota» non
è un errore: l'operando (o la risalita di `-p`) si ferma semplicemente
lì. Nessun altro rifiuto è tollerato. Il primo fallimento reale
arresta l'esecuzione prima di ogni operando successivo. `--` termina
l'analisi delle opzioni: ogni argomento successivo è un percorso.

## OPTIONS

- `-p, --parents` — rimuovere anche gli antenati di ogni operando, dal
  più interno al più esterno.
- `-v, --verbose` — segnalare ogni tentativo di rimozione come
  `rmdir: removing directory, 'dir'`.
- `--ignore-fail-on-non-empty` — una directory non vuota non è un
  errore; con `-p` la risalita si ferma lì.
- `-h, -?` — mostrare la guida breve di questo comando (anche
  `--help`).

## EXAMPLES

- `rmdir Scratch` — rimuovere una directory vuota.
- `rmdir -p Projects/os/build` — rimuovere la catena, dal più interno
  al più esterno.
- `rmdir -p --ignore-fail-on-non-empty a/b` — rimuovere `a/b`, e anche
  `a` se così resta vuota.

## EXIT STATUS

- `0` — ogni rimozione è riuscita (un rifiuto tollerato da
  `--ignore-fail-on-non-empty` non è un fallimento).
- `1` — un errore del filesystem o dell'output; la ragione è stampata
  sullo standard error.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un tag BCP-47 come
  `it-IT`).

## SEE ALSO

mkdir, rm, ls
