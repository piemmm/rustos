## NAME

ls — rhestru cynnwys cyfeiriaduron

## SYNOPSIS

`ls [-aAbCcdFfghilmNnopQqrRsStUuvXx1] [-w cols] [--time=WORD]`
`[--time-style=STYLE] [--sort=WORD] [--quoting-style=STYLE]`
`[--full-time] [--group-directories-first] [--] [path...]`

## DESCRIPTION

Mae'n rhestru pob operand llwybr: darllenir a rhestrir cofnodion
operand cyfeiriadur (oni bai fod `-d` yn enwi'r cyfeiriadur ei hun),
a rhestrir unrhyw operand arall fel ef ei hun. Heb operand, rhestrir
y cyfeiriadur cyfredol (`.`).

Trefnir y cofnodion yn ôl enw (neu yn ôl maint, y mwyaf yn gyntaf,
gydag `-S`; yn ôl stamp amser, y diweddaraf yn gyntaf, gydag `-t`;
gwrthdroir gydag `-r`), un enw fesul llinell yn
ragosodedig. Cuddir cofnodion y mae eu henw'n dechrau â `.` oni roddir
`-a` neu `-A`; pan guddir cofnodion, cyhoeddir nodyn ar y ffrwd
gwybodaeth safonol (fd 3), byth yn y rhestriad ei hun.

Mae'r fformat hir (`-l`) yn dangos y didau math a chaniatâd, y
perchennog a'r grŵp, y maint, yna'r enw. IDau rhifol yw'r perchennog
a'r grŵp: byddai datrys enwau cyfrifon yn gofyn am y gronfa ddata
defnyddwyr a warchodir gan allu, na ddylai rhestriad ei mynnu, felly
mae'r allbwn yn cyfateb i ateb rhifol yr offeryn GNU (mae `-n` yn
rendro'n unfath). Mae'r golofn stamp amser yn dangos yr amser addasu
yn ragosodedig; mae `-c`, `-u` a `--time` yn dewis pa un o'r pedwar
stamp a ddangosir (ac a ddefnyddir i drefnu), ac mae `--time-style`
(neu `--full-time`) yn gosod ei fformat. Nid oes colofn cyfrif
cysylltau eto am nad yw cytundeb y system ffeiliau'n cario cysylltau
caled eto; ymddengys pan wnaiff.

Pan roddir mwy nag un operand — a bob amser o dan `-R` — rhagflaenir
rhestriad pob cyfeiriadur gan bennawd `path:`, a gwahenir y blociau â
llinell wag.

## OPTIONS

- `-t` — didoli yn ôl y stamp amser a ddangosir, y diweddaraf yn
  gyntaf.
- `-c` — defnyddio amser newid metadata (ctime): gydag `-l` ei
  ddangos a chydag `-t` didoli yn ôl hynny; heb `-l`, didoli yn ôl
  hynny.
- `-u` — fel `-c`, ond yr amser cyrchu (atime).
- `-i, --inode` — argraffu rhif nod pob cofnod.
- `--time=WORD` — pa stamp amser i'w ddangos ac i drefnu yn ôl:
  `atime` (`access`, `use`), `ctime` (`status`), `mtime`
  (`modification`) neu `birth` (`creation`).
- `--time-style=STYLE` — fformat y stamp amser: `locale` (rhagosodiad),
  `long-iso`, `full-iso`, `iso`. Ni chefnogir `+FORMAT` personol.
- `--full-time` — fel `-l --time-style=full-iso`.
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
- `-m` — enwau wedi'u gwahanu â choma, wedi'u lapio i'r lled.
- `-n, --numeric-uid-gid` — fformat hir gyda pherchennog a grŵp
  rhifol; mae'n awgrymu `-l`. Mae'r perchennog a'r grŵp bob amser yn
  rhifol yma (gweler uchod), felly mae'n cyfateb i `-l`.
- `-o` — fformat hir heb golofn y grŵp; mae'n awgrymu `-l`.
- `-p` — atodi `/` at gyfeiriaduron.
- `-N, --literal` — argraffu enwau'n llythrennol, heb ddyfynnu
  (`--quoting-style=literal`).
- `-Q, --quote-name` — dyfynnu arddull C: rhoi pob enw mewn dyfynodau
  dwbl, gan ddianc dyfynodau, ôl-slaesau a nodau rheoli
  (`--quoting-style=c`).
- `-b, --escape` — fel `-Q` ond heb y dyfynodau amgylchynol a chyda
  bylchau wedi'u dianc (`--quoting-style=escape`).
- `--quoting-style=WORD` — sut y dyfynnir enwau: `literal` (`-N`),
  `shell`, `shell-always`, `shell-escape`, `shell-escape-always`,
  `c` (`-Q`), neu `escape` (`-b`). Y rhagosodiad yw `shell-escape`
  wrth derfynell a `literal` fel arall; ni chefnogir yr arddulliau
  `locale` a `clocale`.
- `-q, --hide-control-chars` — dangos nodau annraffig fel `?` (y
  rhagosodiad wrth derfynell); yn effeithio ar yr arddulliau nad
  ydynt yn dianc yn unig.
- `--show-control-chars` — argraffu nodau annraffig fel y maent (y
  rhagosodiad pan nad terfynell yw'r allbwn).
- `-r, --reverse` — gwrthdroi trefn y didoli.
- `-R, --recursive` — rhestru is-gyfeiriaduron yn ailadroddus.
- `-s, --size` — argraffu maint neilltuedig pob cofnod mewn blociau
  1024-beit (wedi'i raddio gan `-h`), gyda llinell `total` fesul
  rhestriad cyfeiriadur.
- `-C` — rhestru mewn colofnau, wedi'u llenwi o'r brig i'r gwaelod
  (rhagosodiad ar derfynell).
- `-S` — didoli yn ôl maint, y mwyaf yn gyntaf.
- `-U` — peidio â didoli; rhestru cofnodion yn nhrefn y cyfeiriadur.
- `-X` — didoli yn ôl estyniad enw (y testun o'r `.` olaf),
  cydraddoldeb yn ôl enw.
- `-v` — didoli «fersiwn» naturiol, fel bod `f2` o flaen `f10`;
  cydraddoldeb yn ôl enw.
- `-f` — peidio â didoli a dangos pob cofnod: yn galluogi `-a` a `-U`
  ac yn analluogi `-l` a `-s`. Cymhwysir yn ei safle, felly bydd
  `-l`/`-s`/baner ddidoli ddiweddarach yn ei ddisodli.
- `--sort=WORD` — dewis yr allwedd ddidoli yn ôl enw: `none` (`-U`),
  `size` (`-S`), `time` (`-t`), `version` (`-v`), `extension` (`-X`),
  neu `name`.
- `--group-directories-first` — rhestru cyfeiriaduron cyn cofnodion
  eraill; cyfeiriaduron yn gyntaf hyd yn oed gyda `-r`.
- `-w, --width <cols>` — gosod lled yr allbwn mewn colofnau; mae `0`
  yn golygu diderfyn.
- `-x` — rhestru mewn colofnau, wedi'u llenwi o'r chwith i'r dde.
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
