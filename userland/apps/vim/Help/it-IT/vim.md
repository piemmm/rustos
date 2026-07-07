## NAME

vim — l'editor di testo modale

## SYNOPSIS

`vim [-R] [+num | + | +/pattern] [--] [file ...]`

## DESCRIPTION

Modifica file di testo con l'insieme di comandi modale del celebre
editor vim. La sessione parte in modo normale: i tasti sono comandi, e
`i` (oppure `a`, `o` e le loro varianti) entra nel modo inserimento,
dove ciò che si digita diventa testo. `Esc` torna al modo normale.
`:q` esce; `:wq` (o `ZZ`) scrive ed esce.

Si possono nominare più file; la sessione apre il primo e `:n` /
`:prev` scorrono l'elenco degli argomenti. Un file non ancora
esistente è un `[New File]`, creato alla prima scrittura.

Comandi del modo normale (il nucleo di vim realizzato):

- Movimenti: `h j k l`, le frecce, `w W b B e E`, `0 ^ $`, `f F t T`
  con ripetizione `;`/`,`, `gg G`, `{ }`, `%`, `H M L` e `Enter`. Un
  prefisso numerico ripete un movimento: `3w`.
- Operatori: `d` (cancellare), `c` (cambiare), `y` (copiare),
  applicati su qualunque movimento od oggetto testuale (`iw aw i( a(
  i[ i{ i" i' i<` e le loro coppie); raddoppiati (`dd cc yy`) agiscono
  su righe intere. Scorciatoie: `x X s S D C Y r ~ J`.
- Registri: `"a`–`"z` prima di un operatore o di un incolla sceglie un
  registro con nome; le maiuscole accodano. `p`/`P` incolla dopo/prima
  del cursore.
- Cronologia: `u` annulla modifiche intere, `Ctrl-R` le ripristina, e
  `.` ripete l'ultima modifica (testo inserito compreso).
- Ricerca: `/pattern` in avanti, `?pattern` all'indietro, `n`/`N`
  ripetono, `*` cerca la parola sotto il cursore. I modelli accettano
  letterali, `.`, `*`, `^`, `$`, le classi `[...]` e i confini di
  parola `\<` `\>`. Le occorrenze restano evidenziate fino a `:noh`.
- Selezione visuale: `v` (caratteri) e `V` (righe), estesa con
  qualunque movimento od oggetto testuale, poi trattata con
  `d x c s y J`.
- Scorrimento: `Ctrl-D Ctrl-U` (mezza finestra), `Ctrl-F Ctrl-B` e
  PagSu/PagGiù (finestra intera); `Ctrl-G` mostra il riepilogo del
  file.

Il nucleo ex (`:`): `:w [file]`, `:q`, `:wq`, `:x`, `:e file`,
`:enew`, `:r file`, `:n`, `:prev`, `:noh`, `:set number` /
`:set nonumber`, gli indirizzi di riga (`:12`, `:$`, `:.+2`),
`:[range]d` e `:[range]s/pattern/replacement/[g]` (con `&` per
l'occorrenza intera nella sostituzione, `%` per tutte le righe
dell'intervallo). Un `!` dopo `w`, `q` o `e` forza nonostante la sola
lettura o le modifiche non scritte.

Tutto ciò che vim offre oltre questo nucleo è previsto per fasi
successive; l'elenco vive in `plans/VIM.md` dell'albero dei sorgenti.

## OPTIONS

- `-R` — sola lettura: il buffer si modifica in memoria, ma `:w` è
  rifiutato salvo forzatura con `:w!`.
- `+num` — iniziare alla riga `num` del primo file.
- `+` — iniziare all'ultima riga del primo file.
- `+/pattern` — iniziare alla prima occorrenza di `pattern` nel primo
  file.
- `--` — fine delle opzioni; ogni argomento successivo è un nome di
  file.
- `-h, -?` — mostrare la guida breve propria di questo comando e
  uscire.

## EXIT STATUS

- `0` — la sessione è terminata con un comando di uscita, oppure è
  stata mostrata la guida breve.
- `1` — il terminale ha fallito; il motivo è stampato sull'uscita
  d'errore.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).
- `TERM` — il profilo di terminale della sessione; i valori
  sconosciuti degradano alla base semplice.

## SEE ALSO

- `man`
- `cat`
