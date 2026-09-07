## NAME

flock — rhedeg gorchymyn wrth ddal clo ffeil cynghorol

## SYNOPSIS

`flock [options] ffeil gorchymyn [ymresymiad...]`

## DESCRIPTION

Yn cymryd clo cynghorol sy'n cwmpasu'r `ffeil` gyfan, yn rhedeg y
`gorchymyn` wrth ei ddal, ac yn gorffen â statws y gorchymyn ei hun. Nid
yw dau rediad sy'n enwi'r un `ffeil` byth yn gorgyffwrdd, sef yr hyn sydd
angen ar sgript i gadw un copi ohoni ei hun yn rhedeg ar y tro.

Mae'r clo yn perthyn i'r ffeil agored y mae'r rhediad hwn yn ei dal, felly
mae'r system yn ei ryddhau pan ddaw'r rhediad i ben — trwy orffen, trwy
gael ei ymyrryd, neu trwy chwalu. Nid ysgrifennir dim i'r `ffeil` ac nid
oes dim i'w lanhau wedyn, felly nid oes clo hen yn aros i'r rhediad nesaf
ei ddisgwyl. Crëir y `ffeil` os nad yw'n bod.

Mae cloeon yn **gynghorol**: maent yn cydlynu rhaglenni sy'n cytuno i'w
defnyddio. Nid ydynt yn caniatáu nac yn gwrthod mynediad, felly nid yw
rhaglen nad yw'n cymryd clo yn cael ei rhwystro rhag darllen nac
ysgrifennu. Perchennog, caniatâdau a rhestr fynediad y ffeil sy'n
penderfynu pwy gaiff ddarllen neu ysgrifennu, yn union fel ar gyfer unrhyw
ffeil arall.

Heb `-n` na `-w` mae'r rhediad yn aros am gyhyd ag y bo angen. Gyda `-n`
mae'n ildio ar unwaith; gyda `-w` ar ôl yr amser a roddwyd. Y naill ffordd
neu'r llall, mae ildio'n gorffen â'r cod gwrthdaro (`1` oni bai bod `-E`
yn dweud fel arall) ac **nid** yw'r gorchymyn yn rhedeg — felly gall sgript
bob amser wahaniaethu rhwng "ni chefais y clo" a "methodd y gorchymyn".

Mae tri opsiwn o `flock` systemau eraill yn fwriadol absennol yn lle cael
eu derbyn a'u hanwybyddu. Mae `-u` a `-o` yn gweithredu ar ddisgrifydd
ffeil a agorwyd ymlaen llaw gan gragen, ffurf nad yw'r gorchymyn hwn yn ei
chynnig; mae `-c` yn trosglwyddo ei ymresymiad i gragen, sy'n cael ei
sillafu yma'n benodol fel `flock ffeil elsh -c '...'` fel bod yn eglur pa
gragen sy'n rhedeg. Mae gofyn am unrhyw un ohonynt yn wall defnydd: rhoddir
gwybod i'r sgript yn lle ei rhedeg heb y clo y gofynnodd amdano.

## OPTIONS

- `-s, --shared` — cymryd clo a rennir. Gall sawl rhediad ei ddal ar yr un pryd, ac mae pob un yn eithrio rhediad sy'n gofyn am un unigryw. Dyma glo'r darllenwyr.
- `-x, --exclusive` — cymryd clo unigryw, sy'n eithrio pob deiliad arall. Y rhagosodiad, a chlo'r ysgrifenwyr.
- `-n, --nonblock` — peidio ag aros: os yw'r clo wedi'i ddal, gorffen ar unwaith â'r cod gwrthdaro.
- `-w, --timeout <seconds>` — aros am hyd at y nifer hwn o eiliadau cyfan, wedyn gorffen â'r cod gwrthdaro.
- `-E, --conflict-code <n>` — y statws i orffen â hi pan fydd `-n` neu `-w` yn ildio. `1` yn rhagosodedig. Defnyddiwch werth nad yw'r gorchymyn ei hun byth yn ei ddychwelyd os oes rhaid i sgript wahaniaethu rhyngddynt.
- `-v, --verbose` — adrodd ar y llif gwallau safonol a gymerwyd y clo.
- `-?, --help` — dangos help byr y gorchymyn hwn.

## EXAMPLES

- `flock /Users/ian/Library/backup.lock backup-now` — rhedeg y copi wrth
  gefn, gan aros os yw copi arall yn rhedeg eisoes.
- `flock -n /Storage/db/data.lock compact` — cywasgu'r gronfa ddata, neu
  orffen â `1` ar unwaith os yw'r clo wedi'i ddal.
- `flock -s -w 30 /Storage/db/data.lock report` — cymryd clo darllenydd,
  gan aros hyd at ddeg ar hugain eiliad i ysgrifennwr orffen.
- `flock -E 99 -n lock task` — gorffen â `99` yn lle `1` pan fydd y clo
  wedi'i ddal, fel y gall y sgript wahaniaethu hynny oddi wrth fethiant
  `task`.

## EXIT STATUS

- statws y gorchymyn ei hun — cymerwyd y clo a rhedodd y gorchymyn.
- y cod gwrthdaro (`1` yn rhagosodedig) — ildiodd `-n` neu `-w`; ni
  rhedodd y gorchymyn.
- `1` — ni fu modd cymryd y clo am reswm na fyddai aros yn ei drwsio, neu
  ni fu modd rhedeg y gorchymyn; argreffir y rheswm ar y llif gwallau
  safonol.
- `2` — ni ddeallwyd y llinell orchymyn; ni chlowyd dim ac ni rhedodd dim.
- `126` — canfuwyd y gorchymyn ond ni fu modd ei redeg.
- `127` — ni chanfuwyd y gorchymyn.

## ENVIRONMENT

- `PATH` — chwilir amdano am y gorchymyn, ar ôl storfeydd rhaglenni'r
  system a'r defnyddiwr.
- `HOME` — yn lleoli storfeydd rhaglenni'r defnyddiwr ei hun.
- `LANG` — y locale a ffefrir ar gyfer yr help byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

elsh, ps, ulimit
