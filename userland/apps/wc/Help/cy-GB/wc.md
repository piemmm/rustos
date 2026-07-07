## NAME

wc — argraffu cyfrifon llinellau, geiriau a beitiau pob ffeil

## SYNOPSIS

`wc [option...] [file...]`

`wc [option...] --files0-from <file>`

## DESCRIPTION

Mae'n cyfrif, ar gyfer pob `file`, ei linellau (nodau llinell newydd),
ei eiriau a'i feitiau, ac yn eu hargraffu mewn un rhes ac enw'r ffeil
ar ei hôl. Heb `file`, neu pan fo `file` yn `-`, darllenir y mewnbwn
safonol (ac ni argreffir enw yn y ffurf heb operand). Gyda mwy nag un
mewnbwn, argreffir rhes `total` derfynol yn ôl dewis `--total`.

Mae'r dewiswyr `-l`, `-w`, `-m`, `-c` ac `-L` yn dewis pa gyfrifon a
argreffir; heb yr un, argreffir cyfrifon y llinellau, y geiriau a'r
beitiau. Ymddengys y cyfrifon bob amser yn y drefn sefydlog: llinellau,
geiriau, nodau, beitiau, lled llinell mwyaf. Gair yw rhediad mwyaf o
nodau nad ydynt yn ofod gwyn. Mae `-m` yn cyfrif nodau UTF-8 (mae beit
nad yw'n UTF-8 dilys yn cyfrif fel beit ond nid fel nod); mae `-L` yn
mesur lled arddangos pob llinell mewn colofnau terfynell, gyda thabiau
yn symud i'r lluosrif nesaf o 8.

Mae `--files0-from <file>` yn darllen y rhestr operandau, wedi'i
gwahanu â NUL, o `file` (ystyr `-` yw'r mewnbwn safonol); ni ellir ei
gyfuno ag operandau `file`.

Adroddir am fewnbwn na ellir ei ddarllen ar y gwall safonol ac mae'r
rhediad yn parhau gyda'r mewnbwn nesaf.

## OPTIONS

- `-c, --bytes` — argraffu cyfrif y beitiau.
- `-m, --chars` — argraffu cyfrif y nodau.
- `-l, --lines` — argraffu cyfrif y llinellau newydd.
- `-w, --words` — argraffu cyfrif y geiriau.
- `-L, --max-line-length` — argraffu lled arddangos mwyaf llinell.
- `--files0-from <file>` — darllen y rhestr operandau wedi'i gwahanu â
  NUL o `file` (mae `-` yn ei darllen o'r mewnbwn safonol).
- `--total <when>` — pryd i argraffu'r rhes `total`: `auto` (y
  rhagosodiad: dim ond gyda mwy nag un mewnbwn), `always`, `only` (y
  cyfanswm yn unig, heb label) neu `never`.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `wc notes.txt` — argraffu cyfrifon llinellau, geiriau a beitiau
  `notes.txt`.
- `wc -l a b` — argraffu cyfrif llinellau `a` a `b`, yna'r cyfanswm.
- `wc -L table.txt` — argraffu llinell letaf `table.txt` mewn colofnau
  terfynell.
- `wc -c --total=only a b` — argraffu dim ond cyfrif y beitiau wedi'i
  symio.

## EXIT STATUS

- `0` — cyfrifwyd pob mewnbwn (neu ysgrifennwyd y cymorth byr).
- `1` — ni ellid darllen mewnbwn, neu ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `cat`
- `head`
- `man`
