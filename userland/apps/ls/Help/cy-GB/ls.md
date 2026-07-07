## NAME

ls — rhestru cynnwys cyfeiriaduron

## SYNOPSIS

`ls [-aAdFghlmnopQrRsS1] [--] [path...]`

## DESCRIPTION

Mae'n rhestru pob operand llwybr: darllenir a rhestrir cofnodion
operand cyfeiriadur (oni bai fod `-d` yn enwi'r cyfeiriadur ei hun),
a rhestrir unrhyw operand arall fel ef ei hun. Heb operand, rhestrir
y cyfeiriadur cyfredol (`.`).

Trefnir y cofnodion yn ôl enw (neu yn ôl maint, y mwyaf yn gyntaf,
gydag `-S`; gwrthdroir gydag `-r`), un enw fesul llinell yn
ragosodedig. Cuddir cofnodion y mae eu henw'n dechrau â `.` oni roddir
`-a` neu `-A`; pan guddir cofnodion, cyhoeddir nodyn ar y ffrwd
gwybodaeth safonol (fd 3), byth yn y rhestriad ei hun.

Mae'r fformat hir (`-l`) yn dangos y didau math a chaniatâd, y
perchennog a'r grŵp, y maint, yna'r enw. IDau rhifol yw'r perchennog
a'r grŵp: byddai datrys enwau cyfrifon yn gofyn am y gronfa ddata
defnyddwyr a warchodir gan allu, na ddylai rhestriad ei mynnu, felly
mae'r allbwn yn cyfateb i ateb rhifol yr offeryn GNU (mae `-n` yn
rendro'n unfath). Nid oes colofn cyfrif cysylltau na stampiau amser am
nad yw cytundeb y system ffeiliau'n cario cysylltau caled na stampiau
amser eto; ymddengys y colofnau pan wnaiff.

Pan roddir mwy nag un operand — a bob amser o dan `-R` — rhagflaenir
rhestriad pob cyfeiriadur gan bennawd `path:`, a gwahenir y blociau â
llinell wag.

## OPTIONS

- `-a, --all` — peidio â chuddio cofnodion y mae eu henw'n dechrau â
  `.`.
- `-A, --almost-all` — fel `-a`, ond peidio byth â rhestru `.` na
  `..`.
- `-d, --directory` — rhestru'r operandau cyfeiriadur eu hunain, nid
  eu cynnwys.
- `-F, --classify` — atodi `/` at gyfeiriaduron a `*` at rai
  gweithredadwy.
- `-g` — fformat hir heb golofn y perchennog; mae'n awgrymu `-l`.
- `-h, --human-readable` — gydag `-l`, argraffu meintiau fel `1.1K`,
  `23M` (pwerau 1024).
- `-l` — fformat hir: didau caniatâd, perchennog, grŵp, maint, yna'r
  enw.
- `-m` — enwau wedi'u gwahanu â choma ar un llinell.
- `-n, --numeric-uid-gid` — fformat hir gyda pherchennog a grŵp
  rhifol; mae'n awgrymu `-l`. Mae'r perchennog a'r grŵp bob amser yn
  rhifol yma (gweler uchod), felly mae'n cyfateb i `-l`.
- `-o` — fformat hir heb golofn y grŵp; mae'n awgrymu `-l`.
- `-p` — atodi `/` at gyfeiriaduron.
- `-Q, --quote-name` — rhoi pob enw mewn dyfynodau dwbl, gan ddianc
  dyfynodau, ôl-slaesau a nodau rheoli.
- `-r, --reverse` — gwrthdroi trefn y didoli.
- `-R, --recursive` — rhestru is-gyfeiriaduron yn ailadroddus.
- `-s, --size` — argraffu maint neilltuedig pob cofnod mewn blociau
  1024-beit (wedi'i raddio gan `-h`), gyda llinell `total` fesul
  rhestriad cyfeiriadur.
- `-S` — didoli yn ôl maint, y mwyaf yn gyntaf.
- `-1` — un enw fesul llinell (y rhagosodiad).
- `-?` — dangos cymorth byr y gorchymyn hwn ei hun (`--help` yw'r
  ffurf hir).

## EXAMPLES

- `ls` — rhestru'r cyfeiriadur cyfredol.
- `ls -al /System` — rhestriad fformat hir o `/System`, gan gynnwys y
  cofnodion cudd.
- `ls -lhS` — fformat hir, meintiau darllenadwy, y mwyaf yn gyntaf.
- `ls -R Documents` — mynd trwy `Documents` yn ailadroddus, un pennawd
  fesul cyfeiriadur.
- `ls -F` — marcio cyfeiriaduron â `/` a rhai gweithredadwy â `*`.
- `ls -d Documents` — rhestru'r cofnod `Documents` ei hun, nid ei
  gynnwys.

## EXIT STATUS

- `0` — rhestrwyd pob operand.
- `1` — ni ellid archwilio operand, ni ellid darllen cyfeiriadur, neu
  ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `cat`
- `man`
