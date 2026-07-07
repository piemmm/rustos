## NAME

head — allbynnu rhan gyntaf ffeiliau

## SYNOPSIS

`head [option...] [file...]`

## DESCRIPTION

Mae'n argraffu 10 llinell gyntaf pob `file` i'r allbwn safonol. Gyda
mwy nag un `file`, rhagflaenir pob rhan gan bennawd `==> file <==`.
Heb `file`, neu pan fo `file` yn `-`, darllenir y mewnbwn safonol.

Mae `-n` ac `-c` yn newid faint a argraffir: mae cyfrif plaen yn
argraffu'r `num` llinell neu feit cyntaf; mae cyfrif a ysgrifennir â
`-` blaen yn argraffu popeth **ac eithrio**'r `num` llinell neu feit
olaf. Gall cyfrif gario ôl-ddodiad lluosydd: `b` (512), `kB` (1000),
`K` (1024), `MB`, `M`, `GB`, `G`, ac yn y blaen ar gyfer `T`, `P`,
`E`, `Z`, `Y`, `R`, `Q` (mae llythyren unigol yn lluosi â phwerau
1024; gyda `B` â phwerau 1000; gydag `iB` â phwerau 1024).

Derbynnir ffurf hanesyddol yr ymresymiad cyntaf `head -num` (gyda
lluosyddion `b`/`k`/`m` terfynol a llythrennau `l`/`q`/`v`/`z`
dewisol), fel yn offeryn GNU.

Adroddir am ffeil na ellir ei darllen ar y gwall safonol ac mae'r
rhediad yn parhau gyda'r ffeil nesaf.

## OPTIONS

- `-c, --bytes <num>` — argraffu'r `num` beit cyntaf o bob ffeil;
  gyda `-` blaen, popeth ond y `num` beit olaf.
- `-n, --lines <num>` — argraffu'r `num` llinell gyntaf o bob ffeil;
  gyda `-` blaen, popeth ond y `num` llinell olaf.
- `-q, --quiet, --silent` — peidio byth ag argraffu'r penawdau
  `==> file <==`.
- `-v, --verbose` — argraffu'r penawdau `==> file <==` bob amser.
- `-z, --zero-terminated` — llinellau wedi'u hamffinio â NUL yn lle
  llinell newydd.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `head log.txt` — argraffu 10 llinell gyntaf `log.txt`.
- `head -n 3 a b` — argraffu 3 llinell gyntaf `a` a `b`, pob un o dan
  ei phennawd.
- `head -c 1K image` — argraffu 1024 beit cyntaf `image`.
- `head -n -1 notes` — argraffu `notes` heb ei linell olaf.

## EXIT STATUS

- `0` — argraffwyd pob ffeil (neu ysgrifennwyd y cymorth byr).
- `1` — ni ellid darllen ffeil, neu ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `cat`
- `wc`
- `man`
