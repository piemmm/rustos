## NAME

top — gwylio'r rhestr brosesau'n fyw

## SYNOPSIS

`top [-d secs.tenths] [-h | -?]`

## DESCRIPTION

Mae'n dangos golwg fyw, sgrin-lawn, ar y rhestr brosesau trwy API
Gwybodaeth y System, yn ysbryd `top` GNU. Mae'n dechrau ar brosesau'r
galwr ei hun; dim ond i alwr sy'n dal `CAP_SYSINFO_GLOBAL` y mae'r
gwasanaeth yn caniatáu golwg y system gyfan.

Mae'r arddangosfa'n adnewyddu ei hun bob cyfwng oedi (3.0 eiliad oni
newidia `-d` ef), ac mae `r` yn ei hadnewyddu ar unwaith.

Nid yw'r gwyliwr yn cymryd operandau: fe'i rheolir â bysellau a wesgir
o fewn y sesiwn.

- `q` — gadael.
- `a` — toglo rhwng eich prosesau eich hun a golwg y system gyfan. Os
  gwrthyd y gwasanaeth olwg y system gyfan (mae angen
  `CAP_SYSINFO_GLOBAL` arni), erys y gwyliwr ar eich prosesau eich hun
  a dywed y llinell statws pam; mae'r sesiwn yn dal i redeg.
- `r` — adnewyddu'r rhestriad.
- I fyny/I lawr, PageUp/PageDown, Home/End — symud y dewisiad.
- `h`, `?` — toglo troshaen y bysellau yn y sesiwn.

Mae pedair llinell grynodeb yn rhagflaenu'r rhestr: yr amser ar waith,
cyfrif y defnyddwyr sydd wedi mewngofnodi, a chyfartaleddau llwyth
1/5/15 munud; cyfrifiad y tasgau yn ôl cyflwr; rhaniad defnydd
`%Cpu(s)`; a ffigurau'r cof mewn MiB. Mae angen `CAP_SYSINFO_KERNEL`
ar linell y cof — mae galwr hebddi'n gweld y gwrthodiad wedi'i esbonio
ac mae'r sesiwn yn parhau.

Mae'r llinell `%Cpu(s)` yn dangos y gyfran o'r cyfwng diwethaf a
dreuliodd pob CPU gyda'i gilydd yn brysur (yn rhedeg tasgau) ac yn
segur. Dim ond amser prysur a segur y mae RustOS yn eu cyfrifo, felly
lle mae `top` GNU yn rhannu'r gyfran brysur yn ffigurau
user/system/nice/iowait, mae'r llinell hon yn fwriadol yn dangos y ddau
ffigur real yn eu lle.

Trefnir y rhesi yn ôl `%CPU`, y defnyddiwr mwyaf yn gyntaf, ac maent
yn cario:

- `PID` — id rhifol y broses.
- `USER` — enw defnyddiwr y cyfrif sy'n berchen, wedi'i ddatrys o
  gyfeiriadur cyfrifon y system; saif yr uid rhifol yn ei le pan na
  ellir datrys yr enw.
- `SIZE` — y cof a fapiwyd yng ngofod cyfeiriadau'r broses (delwedd,
  pentwr a thomen fel ei gilydd).
- `S` — llythyren y cyflwr: `R` yn rhedeg (gwyrdd), `r` yn rhedadwy,
  yn aros am CPU (cyan), `S` yn cysgu, `T` wedi'i atal (melyn), `Z`
  zombie (magenta). Dim ond ar derfynell liw yr ymddengys y lliwiau;
  mae'r llythyren ei hun bob amser yn cario'r cyflwr.
- `%CPU` — cyfran y CPU dros y cyfwng ers yr adnewyddiad blaenorol.
- `WCPU` — cyfran bwysol (wedi'i llyfnhau'n esbonyddol) y CPU ar
  draws adnewyddiadau, sefydlocach na'r golofn ennyd.
- `TIME+` — amser CPU cronnus, fel `munudau:eiliadau.canfedau`.
- `COMMAND` — enw'r broses.

## OPTIONS

- `-d, --delay <seconds>` — y cyfwng rhwng adnewyddiadau awtomatig,
  mewn eiliadau gyda ffracsiwn dewisol (dim ond y digid ffracsiynol
  cyntaf, degfedau, a gedwir): mae `top -d 1.5` yn adnewyddu bob 1.5
  eiliad. Y rhagosodiad yw 3.0. Mae `top` GNU yn derbyn oedi sero ac
  yn adnewyddu mor gyflym ag y gall; nid yw RustOS byth yn troelli'n
  brysur, felly clampir sero i'r isafswm 0.1 e.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael. O
  fewn sesiwn sy'n rhedeg mae'r un bysellau'n toglo troshaen y
  bysellau yn lle hynny.

## EXIT STATUS

- `0` — daeth y sesiwn i ben gyda `q`, neu dangoswyd y cymorth byr.
- `1` — methodd y gwasanaeth neu'r derfynell; argraffir y rheswm ar y
  gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
