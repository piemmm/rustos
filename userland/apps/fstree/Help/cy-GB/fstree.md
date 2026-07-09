## NAME

fstree — y rheolwr ffeiliau coeden sgrin lawn

## SYNOPSIS

`fstree [cyfeiriadur]`

## DESCRIPTION

Yn pori'r system ffeiliau mewn sesiwn sgrin lawn a yrrir gan y
bysellfwrdd: panel coeden gyfeiriaduron ar y chwith a phanel ffeiliau ar
y dde sy'n rhestru cofnodion y cyfeiriadur a ddewiswyd gyda'u meintiau
a'u hamserau addasu. Mae'r sesiwn yn dechrau yn `cyfeiriadur` (y golwg
gwraidd `/` yn ddiofyn).

Darllenir y goeden yn ddiog: dim ond pan gaiff ei ddangos neu ei ehangu
am y tro cyntaf y cyrchir cynnwys cyfeiriadur, felly dim ond y
cyfeiriaduron a agorwyd mewn gwirionedd yw cost pori cyfrol enfawr. Caiff
cyfeiriadur na chaiff y galwr ei restru ei wrthod yn y fan a'r lle —
ymddengys y gwall ar y llinell negeseuon a chedwir y golwg blaenorol; ni
chaiff dim ei ffugio.

Bysellau:

- `Fyny`/`Lawr` neu `k`/`j` — symud cyrchwr y panel gweithredol. Mae symud
  cyrchwr y goeden yn rhestru'r cyfeiriadur newydd ei ddewis yn y panel
  ffeiliau.
- `Chwith`/`De` neu `h`/`l` — cau/ehangu rhes y goeden o dan y cyrchwr.
- `Enter` — yn y goeden, toglo'r ehangu; yn y panel ffeiliau, disgyn i'r
  cyfeiriadur a ddewiswyd (mae'r ddau banel yn dilyn).
- `Tab` — newid y panel gweithredol.
- `s` — agor y ddewislen didoli: `n` enw, `e` estyniad, `s` maint,
  `m` amser addasu, `r` gwrthdroi'r cyfeiriad, `Esc` yn canslo. Caiff
  cyfeiriaduron eu grwpio bob amser cyn ffeiliau.
- `c` — copïo'r cofnod a ddewiswyd: mae llinell fewnbwn yn gofyn am y
  gyrchfan. Mae cyrchfan gymharol yn glanio yn y cyfeiriadur a
  restrwyd; mae cyrchfan sy'n gyfeiriadur presennol yn derbyn y copï
  y tu mewn iddo o dan enw'r ffynhonnell. Caiff cyfeiriadur ei gopïo
  gyda phopeth oddi tano. Gwrthodir copïo cofnod ar ei ben ei hun neu
  gyfeiriadur i mewn i'w is-goeden ei hun cyn ysgrifennu dim.
- `m` — symud y cofnod a ddewiswyd, gyda'r un cwestiwn cyrchfan. O
  fewn yr un gyfrol mae'r symud yn ailenwi atomig; ar draws cyfrolau
  caiff y cofnod ei gopïo ac yna dilëir y ffynhonnell.
- `r` — ailenwi'r cofnod a ddewiswyd yn ei le: mae'r llinell fewnbwn
  wedi'i rhag-lenwi â'r enw presennol.
- `d` — dileu'r cofnod a ddewiswyd ar ôl cadarnhad; dim ond `y` sy'n
  bwrw ymlaen. Mae dileu cyfeiriadur yn tynnu popeth oddi tano, ac
  mae'r cadarnhad yn dweud hynny.
- `M` — creu cyfeiriadur yn y cyfeiriadur a restrwyd; gofynnir am ei
  enw.
- `a` — golygu didau caniatâd y cofnod a ddewiswyd: llinell wythol
  wedi'i rhag-lenwi â'r modd presennol. Mae Enter yn cymhwyso (dim ond y
  perchennog all ei newid — mae'r cnewyllyn yn gwrthod pawb arall), mae
  Esc yn canslo.
- `t` — tagio neu ddad-dagio cofnod a ddewiswyd y panel ffeiliau a
  symud rhes i lawr; mae pwyso dro ar ôl tro felly'n tagio rhes o
  gofnodion. Mae cofnodion wedi'u tagio yn cario `*`.
- `T` — tagio yn ôl patrwm: glob (`*`, `?`, `[...]`) a gymherir â'r
  enwau gweladwy; ychwanegir pob cydweddiad at y set a dagiwyd.
- `i` — gwrthdroi'r tagiau dros y cofnodion gweladwy.
- `C` — clirio pob tag.
- `u` — cyfrif defnydd disg o dan y cyfeiriadur â ffocws: ffeiliau,
  beitiau a chyfeiriaduron, wedi'u cerdded fesul cam yn y cefndir.
  Mae `Esc` yn canslo gan gadw'r ffigurau a gyfrifwyd hyd hynny.
- `v` — fflatio'r gangen o dan y cyfeiriadur â ffocws: un rhestr o
  bob ffeil oddi tani, yn llenwi fesul tudalen (mae `Bwlch` yn llwytho'r
  dudalen nesaf). Yn y golwg, mae `t`/`T`/`i`/`C` yn tagio ei rhesi,
  mae `c`/`m`/`d` yn rhedeg gweithredoedd swp dros y set a dagiwyd, ac
  mae `Esc` yn dychwelyd i'r paneli. Enwir y rhesi yn gymharol â'r
  gangen a fflatiwyd.
- `.` — dangos/cuddio cofnodion cudd (enwau â dot) yn y ddau banel.
- `?` — dangos y cymorth hwn dros y paneli; mae unrhyw fysell yn ei gau.
- `q` — gadael, gan adfer y derfynell.

Tra bo cofnodion wedi'u tagio, mae `c`, `m` a `d` yn gweithredu ar y
set gyfan a dagiwyd yn hytrach na'r dewisiad: mae `c`/`m` yn gofyn am
gyfeiriadur cyrchfan presennol y mae'r cofnodion yn glanio ynddo, ac
mae `d` yn cadarnhau'r dileu swp. Prosesir y cofnodion yn nhrefn eu
tagio; nid yw cofnod a fethodd byth yn atal y gweddill, mae'r
adroddiad terfynol yn cyfrif yr hyn a lwyddodd, ac mae sgrin adroddiad
yn enwi pob methiant — nid yw swp byth yn rhannol yn dawel. Caiff
cofnodion a lwyddodd eu dad-dagio; erys methiannau wedi'u tagio ar
gyfer ailgynnig.

Pan fyddai copïo neu symud yn trosysgrifo ffeil bresennol, mae'r
sesiwn yn gofyn fesul ffeil: mae `o` yn trosysgrifo, mae `s` yn hepgor
(erys ffynhonnell a hepgorwyd yn ei lle), ac mae `c` yn canslo'r camau
sy'n weddill — mewn swp, mae canslo'n gollwng pob cofnod sy'n weddill
— erys yr hyn a gymhwyswyd eisoes, ac mae'r adroddiad
terfynol yn dweud beth ddigwyddodd. Mae methiant hanner ffordd drwy
gopïo yn tynnu'r gyrchfan hanner-ysgrifenedig ac yn dangos gwall y
cnewyllyn; nid oes dim byth yn esgus bod yn gopï cyflawn. Caiff pob
gweithred ei hawdurdodi gan y cnewyllyn — ymddengys gwrthodiad air am
air ar y llinell negeseuon heb i ddim newid.

Mae'r llinell statws yn dangos y llwybr a restrwyd, nifer y cofnodion
gweladwy, y drefn didoli, beitiau rhydd/cyfanswm y gyfrol sylfaenol (pan
all y gwasanaeth gwybodaeth system eu hadrodd), a yw cofnodion cudd yn
cael eu dangos, a — tra bo rhywbeth wedi'i dagio — nifer y cofnodion a
dagiwyd gyda'u cyfanswm beitiau. Mae ffeil nad yw ei fformat storio yn
cadw amser addasu
yn dangos `-` yng ngholofn yr amser.

Daw chwilio a'r gwylwyr testun/hecs/dadosodwr mewn camau
diweddarach o gynllun yr offeryn.

## OPTIONS

- `directory` — y cyfeiriadur y mae'r sesiwn yn dechrau ynddo; y
  rhagosodiad yw'r golwg gwraidd `/`.
- `-h`, `-?` — argraffu ffurf fer y ddogfen hon a gadael.

## EXIT STATUS

- `0` — daeth y sesiwn i ben drwy `q` y defnyddiwr.
- `1` — ni ellid rhestru'r cyfeiriadur cychwynnol, neu fethodd llwybr y
  derfynell.
- `2` — ni ellid deall y dadleuon.

## SEE ALSO

ls, cp, mv, rm, mkdir, chmod, du, df, find
