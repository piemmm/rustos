## NAME

tail — dangos rhan olaf ffeiliau

## SYNOPSIS

`tail [option...] [file...]`

## DESCRIPTION

Yn argraffu 10 llinell olaf pob `file` i'r allbwn safonol. Gyda mwy nag
un `file`, mae pennawd `==> file <==` yn rhagflaenu pob rhan. Heb `file`,
neu pan fo `file` yn `-`, darllenir y mewnbwn safonol.

Mae `-n` a `-c` yn newid faint a argreffir: mae cyfrif syml (neu un a
ysgrifennwyd â `-` ar y dechrau) yn argraffu'r `num` llinell neu feit
olaf; mae cyfrif a ysgrifennwyd â `+` ar y dechrau yn argraffu popeth
**o'r** llinell neu feit `num` (gan gyfrif o 1) hyd y diwedd. Gall cyfrif
gario ôl-ddodiad lluosydd: `b` (512), `kB` (1000), `K` (1024), `MB`, `M`,
`GB`, `G`, ac yn y blaen ar gyfer `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (mae
llythyren unigol yn lluosi â phwerau 1024; gyda `B` â phwerau 1000; gyda
`iB` â phwerau 1024).

Derbynnir y ffurf hanesyddol yn arg gyntaf `tail -num` / `tail +num`
(gyda llythyren derfynol `b`/`c`/`l` ddewisol), fel yn yr offeryn GNU.

Mae'r modd dilyn yn cadw pob ffeil ar agor ac yn argraffu data newydd wrth iddi dyfu; mae'n blocio nes i'r ffeil newid — byth aros prysur. Mae `-f` yn dilyn y disgrifydd; mae `-F` yn dilyn yr enw ac yn ailagor ffeil a gylchdrowyd, ac mae `--retry` yn aros i enw ymddangos. Mae `--pid=PID` yn gorffen y dilyn pan fydd y broses yn gorffen (gwirir bob `--sleep-interval` eiliad, rhagosodiad 1; `--max-unchanged-stats` rhagosodiad 5). Adroddir am dorri byr a dilynir y ffeil o'i dechrau newydd.

Pan na ddangosir cynnwys blaen, ysgrifennir cofnod cynghori i'r ffrwd
gwybodaeth safonol (fd 3); nid yw byth yn newid yr allbwn na'r statws
gadael. Adroddir am ffeil na ellir ei darllen ar yr allbwn gwall ac mae'r
rhediad yn parhau â'r ffeil nesaf.

## OPTIONS

- `-c, --bytes <num>` — argraffu `num` beit olaf pob ffeil; gyda `+` ar y
  dechrau, popeth o feit `num` ymlaen.
- `-n, --lines <num>` — argraffu `num` llinell olaf pob ffeil; gyda `+`
  ar y dechrau, popeth o linell `num` ymlaen.
- `-q, --quiet, --silent` — peidio byth ag argraffu'r penawdau
  `==> file <==`.
- `-v, --verbose` — argraffu'r penawdau `==> file <==` bob amser.
- `-z, --zero-terminated` — mae llinellau wedi'u hamffinio gan NUL yn
  lle toriad llinell.
- `-f, --follow[=descriptor]` — dilyn yn ôl y disgrifydd, gan argraffu data a atodwyd.
- `-F` — dilyn yn ôl yr enw (`--follow=name --retry`); ailagor ffeil a gylchdrowyd.
- `--follow=name` — dilyn yr enw yn hytrach na'r disgrifydd.
- `--retry` — dal ati i geisio agor ffeil nad yw'n bresennol eto.
- `--pid <PID>` — atal y dilyn pan fydd y broses `PID` yn marw.
- `--sleep-interval <N>` — eiliadau rhwng y gwiriadau (rhagosodiad 1).
- `--max-unchanged-stats <N>` — cylchoedd tawel cyn i `-F` ail-wirio'r enw (rhagosodiad 5).
- `-h, -?` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `tail log.txt` — argraffu 10 llinell olaf `log.txt`.
- `tail -n 3 a b` — argraffu 3 llinell olaf `a` a `b`, pob un o dan ei
  bennawd.
- `tail -c 1K image` — argraffu 1024 beit olaf `image`.
- `tail -n +5 notes` — argraffu `notes` o'i 5ed llinell.

## EXIT STATUS

- `0` — argraffwyd pob ffeil (neu ysgrifennwyd y cymorth byr).
- `1` — ni ellid darllen ffeil, neu ni ellid cyflenwi'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `head`
- `cat`
- `wc`
- `man`
