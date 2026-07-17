## NAME

sysmon — gwylio cof a llwyth y cnewyllyn yn fyw

## SYNOPSIS

`sysmon [-d eil.degfedau] [-h | -?]`

## DESCRIPTION

Yn dangos golwg sgrin-lawn, fyw, o gof a llwyth y cnewyllyn drwy API
gwybodaeth y system: cof ffisegol, pentwr y cnewyllyn, band pwysau'r
cof gyda'i hanes, cofrestr y storfeydd adferadwy, haen gywasgedig
`ramzip`, cyfanswm y cof wedi'i binio, llwyth pob CPU a chyfrifiad o'r
prosesau. Mae'r offeryn yn parhau'n ddefnyddiadwy dan lwyth bwriadol ac
yn gorffwys rhwng adnewyddiadau pan fo'r system yn segur.

Wrth gychwyn, mae'r monitor yn pinio ei gof ei hun (`mem_pin`, sy'n
gofyn am `CAP_MEM_PIN`) fel na fydd byth yn oedi ar ei fethiannau
tudalen ei hun dan yr union bwysau y mae'n ei wylio. Adroddir am
biniad a wrthodwyd ar linell y teitl ac mae'r sesiwn yn parhau heb
biniad — mae'r piniad yn atodol, byth yn angheuol.

Mae'r sgrin yn adnewyddu ei hun bob cyfwng (3.0 eiliad oni bai bod
`-d` yn ei newid), ac mae `r` yn ei adnewyddu ar unwaith. Nid yw'r
monitor yn cymryd operandau: fe'i rheolir â bysellau o fewn y sesiwn.

- `q` — gadael.
- `p` — cylchu'r panel manylion: storfeydd adferadwy, yr haen
  gywasgedig, llwyth pob CPU, prosesau.
- `r` — adnewyddu nawr.
- `+` / `-` — ymestyn / byrhau'r cyfwng o un eiliad, rhwng 0.1 a 60
  eiliad.
- I fyny/I lawr, PgUp/PgDn, Home/End — sgrolio'r panel.
- `h`, `?` — dangos neu guddio'r trosolwg bysellau.

Mae chwe llinell grynodeb yn rhagflaenu'r panel manylion: y teitl
(amser rhedeg, cyfartaleddau llwyth a chyflwr y piniad); ffigurau'r cof
mewn MiB gyda'r cyfanswm wedi'i binio; band y pwysau gyda'i fesurydd,
ffigurau rhydd/wrth gefn a rhifyddion mynediad; hanes y bandiau (un
glyff fesul adnewyddiad: `.` normal, `-` ysgafn, `=` cymedrol, `#`
difrifol, `!` argyfyngus); llinell gyfun y CPU; a chyfrifiad y tasgau.

Mae pob ffigur yn teithio drwy API gwybodaeth y system — nid oes
`/proc`. Mae ymholiadau ystadegau'r cnewyllyn yn gofyn am
`CAP_SYSINFO_KERNEL`, a'r cyfrifiad o bob proses am
`CAP_SYSINFO_GLOBAL`: i'r sawl sydd heb un ohonynt, esbonnir
gwrthodiad y panel hwnnw tra bo gweddill y sesiwn yn parhau. Gwaith
`top` yw'r rhestr brosesau ryngweithiol lawn; yma dim ond y cyfrifiad
a'r defnyddwyr mwyaf yn ôl `%CPU` a chof a ddangosir.

## OPTIONS

- `-d, --delay <seconds>` — y cyfwng rhwng adnewyddiadau awtomatig,
  mewn eiliadau gyda ffracsiwn dewisol (dim ond y digid degol cyntaf,
  y degfedau, a gedwir): mae `sysmon -d 1.5` yn adnewyddu bob 1.5
  eiliad. Rhagosodiad 3.0. Mae GNU `top` yn derbyn cyfwng sero ac yn
  adnewyddu mor gyflym ag y gall; nid yw TAIRiX byth yn troelli'n
  wag, felly codir sero i'r isafswm o 0.1 eiliad.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn a gadael. O fewn
  sesiwn sy'n rhedeg, mae'r un bysellau'n toglo'r trosolwg bysellau yn
  lle hynny.

## EXIT STATUS

- `0` — daeth y sesiwn i ben gyda `q`, neu dangoswyd y cymorth byr.
- `1` — methodd y terfynell; ysgrifennir y rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
