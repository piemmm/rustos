## NAME

sysinfo — ymholi gwybodaeth y system

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Mae'n cyhoeddi un ymholiad teipiedig i API Gwybodaeth y System ac yn
rendro'r ateb. Nid oes `/proc` na `/sys` gan RustOS: y gorchymyn hwn
yw wyneb terfynell yr un API fersiynedig, a wirir gan alluoedd, y mae
pob rhaglen yn ei ddefnyddio, ac nid oes llwybr yn osgoi gwiriad y
gallu.

Yr ymholiadau:

- `processes`, `ps` — rhestru prosesau, un rhes fesul proses.
- `memory`, `mem` — ystadegau cof y cnewyllyn (angen
  `CAP_SYSINFO_KERNEL`).
- `hardware`, `hw` — y goeden galedwedd a ganfuwyd (angen
  `CAP_SYSINFO_HW`).
- `identity`, `id` — hunaniaeth y peiriant a fersiwn yr OS.
- `uptime` — yr amser ers cychwyn ac amser cloc wal y cychwyn.
- `limits`, `rlimits` — eich terfynau adnoddau effeithiol a'r defnydd
  byw.
- `seats` — rhestr y seddi: perchennog pob dangosydd a'i gonsol
  blaendir (angen `CAP_SYSINFO_HW`).
- `pressure` — y mesurydd pwysau cof byw: band, marciau dŵr a rhifwyr
  trosglwyddo (angen `CAP_SYSINFO_KERNEL`).
- `reclaim` — cofrestr y storfannau adferadwy, un rhes i bob dosbarth
  (angen `CAP_SYSINFO_KERNEL`).
- `ramzip` — rhifwyr yr haen gof gywasgedig (angen
  `CAP_SYSINFO_KERNEL`).
- `cpu` — dyfnder ciw rhedeg, newidiadau cyd-destun a rhagachubion fesul
  CPU (angen `CAP_SYSINFO_KERNEL`).
- `help` — cymorth byr y gorchymyn hwn ei hun.

Heb ymholiad, dangosir y cymorth byr.

## OPTIONS

- `--all, -a` — gyda `processes`: rhestru pob proses ar y system yn
  hytrach na'ch rhai chi'n unig; dim ond i alwr sy'n dal
  `CAP_SYSINFO_GLOBAL` y mae'r gwasanaeth yn caniatáu'r olwg hon.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `sysinfo identity` — argraffu hunaniaeth y peiriant a fersiwn yr
  OS.
- `sysinfo ps --all` — rhestru pob proses ar y system.

## EXIT STATUS

- `0` — atebwyd yr ymholiad a'i rendro.
- `1` — gwrthododd neu fethodd y gwasanaeth, neu ni ellid cyflwyno'r
  canlyniad.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `man`
- `ps`
- `top`
