## NAME

stress — llwytho CPU, cof, disg a storfeydd y peiriant yn ôl y galw

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

Yn lansio prosesau gwaith sy'n llwytho'r peiriant yn fwriadol, yn
ysbryd yr offer sefydledig `stress`/`stress-ng`: dolenni CPU
(`--cpu`), gweithwyr cof dyrannu-a-chyffwrdd (`--vm`),
ysgrifennu/cysoni byfferau bach (`--io`), ysgrifenwyr disg dilyniannol
mawr (`--hdd`) ac ail-ddarllenwyr sy'n corddi'r storfeydd (`--cache`,
ychwanegiad RustOS). Mae pob gweithiwr yn broses gyfnewidiadwy ei hun;
mae'r broses reoli yn pinio ei chof ei hun (`mem_pin`, angen
`CAP_MEM_PIN`) er mwyn aros yn ymatebol o dan y pwysau y mae hi ei hun
yn ei greu, ac yn arsylwi `Ctrl-C`/`Terminate`, fel bod pob diwedd i'r
rhediad — cwblhau, terfyn amser neu signal — yn atal y gweithwyr, yn
eu casglu ac yn dileu pob ffeil waith.

Mesurir targedau cof a disg o'r peiriant ei hun: oni bai bod
`--vm-bytes`/`--hdd-bytes` yn enwi ffigurau penodol, mae'r gweithwyr
vm yn rhannu hanner y RAM a ddarganfuwyd a'r gweithwyr hdd hanner lle
rhydd y gyfrol waith. Mae `--overcommit P` yn ailraddio'r targedau
darganfyddedig hynny i `P` y cant o'r adnodd; dros 100 mae'r gweithwyr
yn gwthio i'r pwysau, ac mae'r gwrthodiadau teipiedig a gynhyrchir
(cyfrol lawn, terfyn adnoddau) yn cael eu cyfrif a'u hadrodd fel
canlyniadau disgwyliedig — byth yn ailgeisio, byth yn chwalu. Nid oes
angen braint ar lwytho'r peiriant y tu hwnt i derfynau adnoddau'r
galwr ei hun — y terfynau yw'r amddiffyniad, ac mae `stress` yn eu
parchu.

Dim ond o dan y cyfeiriadur gwaith y mae gweithwyr sy'n cyffwrdd â'r
ddisg yn ysgrifennu — cyfeiriadur storfa'r defnyddiwr ar gyfer yr ap
(`$HOME/Library/stress`) oni bai bod `--temp-path` yn enwi un arall —
a dilëir pob ffeil waith wrth ddatgymalu, gan gynnwys ar lwybrau'r
signalau.

Argreffir crynodeb pan ddaw'r rhediad i ben (a atalir gan `--quiet`),
ac allyrrir cofnod `summary` darllenadwy gan beiriant ar y ffrwd
gwybodaeth safonol gynghorol (fd 3).

## OPTIONS

- `--cpu N`, `--io N`, `--vm N`, `--hdd N` — lansio `N` gweithiwr o'r
  math a enwyd, gydag ystyr GNU `stress`.
- `--cache N` — lansio `N` corddwr storfa (RustOS yn unig: mae
  teithiau oer ailadroddus drwy gyfeiriaduron ac ail-ddarlleniadau yn
  symud cofrestrau storfeydd adferadwy'r cnewyllyn).
- `--all N` — `N` gweithiwr o bob math.
- `--vm-bytes B`, `--hdd-bytes B` — targed beit pob gweithiwr, gyda'r
  ôl-ddodiaid GNU (`k`, `m`, `g`, `t`; e.e. `256M`). Mesurir y
  rhagosodiadau o'r RAM / lle rhydd a ddarganfuwyd.
- `--overcommit P` — graddio'r targedau vm/hdd darganfyddedig i `P` y
  cant o'r adnodd; caniateir mynd dros 100 (mae gwrthodiadau wedyn yn
  ganlyniadau disgwyliedig).
- `--timeout T` — aros ar ôl `T` (ôl-ddodiaid `s`/`m`/`h`; e.e.
  `5m`). Dim rhagosodiad: hebddo mae'r rhediad yn parhau nes i signal
  ddod ag ef i ben.
- `--temp-path DIR` — cyfeiriadur gwaith y gweithwyr sy'n cyffwrdd
  â'r ddisg.
- `--monitor` — rhedeg `sysmon` yn y blaendir am y cyfnod; adroddir
  am y rhediad pan fydd y monitor yn gadael. Yn gwrth-ddweud
  `--background`.
- `-q, --quiet` — atal y crynodeb a'r llinellau cynnydd ar stdout
  (mae gwallau'n dal i gyrraedd stderr).
- `--background` — argraffu PID y rheolydd datgysylltiedig a
  dychwelyd yr anogwr (yn awgrymu `--quiet`). Mae ffurf `&` y gragen
  yn gweithio hefyd; ar gyfer sgriptiau y mae'r faner hon.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn a gadael.
- `--version` — argraffu enw a fersiwn yr offeryn a gadael.

## EXIT STATUS

- `0` — cwblhawyd y rhediad (mae gwrthodiadau teipiedig y gweithwyr
  yn ganlyniadau disgwyliedig ac nid ydynt yn ei fethu).
- `1` — methodd gweithiwr go iawn, neu ni ellid paratoi'r rhediad.
- `2` — ni ddeallwyd y llinell orchymyn.
- `130` / `143` — daeth `Ctrl-C` / `Terminate` â'r rhediad i ben, ar
  ôl datgymalu'r gweithwyr a dileu'r ffeiliau gwaith.

## ENVIRONMENT

- `HOME` — yn lleoli'r cyfeiriadur gwaith rhagosodedig
  (`$HOME/Library/stress`).
- `LANG` — hoff locale y cymorth byr (tag BCP-47 fel `cy-GB`).

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
