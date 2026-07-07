## NAME

useradd — creu cyfrif defnyddiwr

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

Mae'n ychwanegu un cyfrif at gronfa ddata'r defnyddwyr. Rhaid i'r enw
mewngofnodi gyfateb i `[a-z_][a-z0-9_-]*`; mae'r grŵp cynradd (`-g`) yn
ofynnol ac id degol yw pob cyfeiriad at grŵp neu ddefnyddiwr. Gweithred
weinyddol yw creu cyfrif: mae'r gronfa ddata'n gwrthod galwr heb allu
gweinyddu defnyddwyr.

Nid oes gan y cyfrif a grëwyd **gyfrinair y gellir ei ddefnyddio**: nid
oes cyfrinair yn cyfateb iddo nes i weinyddwr osod un (ac ni ellir
dyfalu'r un), yn union fel y mae offeryn GNU yn creu cyfrif
analluogedig. Gosodwch gyfrinair wedyn gyda gorchymyn `passwd` offeryn
`users`.

Pan hepgorir `-u`, dyrennir id y defnyddiwr yn awtomatig, un uwchlaw'r
id uchaf sy'n bodoli. Pan hepgorir `-d`, y cyfeiriadur cartref yw
cynllun safonol `/Users/NAME`. Mae'r cyfrif yn dechrau â chragen
ragosodedig y system a nenfwd cyffredin galluoedd y sesiwn; mae
gweinyddwr yn ei ledu wedyn gyda gorchymyn `grant` offeryn `users`.

Mae `--` yn gorffen dosrannu opsiynau: mae pob ymresymiad diweddarach
yn operand.

## OPTIONS

- `-u, --uid UID` — id rhifol y defnyddiwr; fe'i dyrennir yn awtomatig
  pan hepgorir (un uwchlaw'r id uchaf sy'n bodoli).
- `-g, --gid GID` — id rhifol y grŵp cynradd. Gofynnol: nid oes polisi
  grŵp rhagosodedig i'w ddyfalu.
- `-G, --groups LIST` — idau rhifol grwpiau atodol wedi'u gwahanu â
  choma.
- `-c, --comment TEXT` — sylw'r cyfrif / enw arddangos llawn.
- `-d, --home PATH` — y cyfeiriadur cartref; `/Users/NAME` pan
  hepgorir.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `useradd -g 100 alice` — creu `alice` yn y grŵp cynradd `100` gydag
  id a ddyrannwyd yn awtomatig.
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — pob maes
  wedi'i sillafu.

## EXIT STATUS

- `0` — crëwyd y cyfrif.
- `1` — gwrthododd neu fethodd y gronfa ddata'r creu (er enghraifft
  gallu coll, id dyblyg neu grŵp anhysbys); argraffir y rheswm ar y
  gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `groupadd`
- `users`
