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
- `.` — dangos/cuddio cofnodion cudd (enwau â dot) yn y ddau banel.
- `?` — dangos y cymorth hwn dros y paneli; mae unrhyw fysell yn ei gau.
- `q` — gadael, gan adfer y derfynell.

Mae'r llinell statws yn dangos y llwybr a restrwyd, nifer y cofnodion
gweladwy, y drefn didoli, beitiau rhydd/cyfanswm y gyfrol sylfaenol (pan
all y gwasanaeth gwybodaeth system eu hadrodd) ac a yw cofnodion cudd yn
cael eu dangos. Mae ffeil nad yw ei fformat storio yn cadw amser addasu
yn dangos `-` yng ngholofn yr amser.

Daw'r gweithrediadau ffeil (copïo, symud, ailenwi, dileu), tagio, chwilio
a'r gwylwyr testun/hecs/dadosodwr mewn camau diweddarach o gynllun yr
offeryn.

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

ls, du, df
