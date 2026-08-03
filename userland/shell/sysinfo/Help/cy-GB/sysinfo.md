## NAME

sysinfo — ymholi gwybodaeth y system

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Mae'n cyhoeddi un ymholiad teipiedig i API Gwybodaeth y System ac yn
rendro'r ateb. Nid oes `/proc` na `/sys` gan TAIRiX: y gorchymyn hwn
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
- `irq`, `irqs` — tabl IRQ y cnewyllyn: un rhes fesul llinell ymyrraeth
  rwymedig — ei rhif, y dasg gyrrwr sy'n berchen arni, nifer yr
  ymyriadau ers cychwyn, ac a yw'r llinell dan gwarantîn (angen
  `CAP_SYSINFO_HW`).
- `cpuinfo` — yr adroddiad prosesydd fesul CPU (uwchset o
  `/proc/cpuinfo`): model a gwneuthurwr, dosbarth perfformiad, baneri
  estyniadau ISA, y gofrestr hunaniaeth grai, cyflymder cloc y craidd a
  fesurwyd yn fyw (mewn MHz — neu «unknown» gonest lle nad oes cownter
  cloc craidd) a'r amledd cyfeirio neu sail amser sefydlog. Ffeithiau
  caledwedd cyhoeddus, nid oes angen unrhyw allu.
- `storage`, `io` — iechyd M/A storio fesul cyfrol: un rhes fesul cyfrol
  bloc sy'n ymwybodol o namau — rhagddodiad ei dynodydd parhaol, pen
  gwasanaeth blociau sy'n ei gwasanaethu, ei argaeledd cyfredol
  (available/degraded/recovering/lost) a'r cownteri canlyniad cronnol
  (cwblhau, ailosodiadau, terfynau amser, gwallau cyfrwng, ailgyhoeddiadau)
  y daw disg sy'n methu neu'n ansefydlog i'r golwg arnynt (angen
  `CAP_SYSINFO_KERNEL`).
- `raid`, `arrays` — yr araeau RAID cyfansawdd a'r dyfeisiau y mae'r
  cyfansoddwr araeau yn eu dal: un rhes fesul arae — rhagddodiad ei
  hunaniaeth, ei lefel, ei iechyd (optimal/degraded/recovering/failed),
  nifer yr aelodau cydamserol a diffiniedig, ei uned stribed, ei nifer o
  flociau, ac unrhyw ailadeiladu neu wirio ar y gweill — wedyn un rhes
  fesul dyfais — ei nod yng nghoeden y caledwedd, yr arae y mae'n perthyn
  iddi (cysylltnod ar gyfer ymgeisydd heb gysylltiad), ei slot, ei rôl
  (candidate/held/in-sync/resyncing/faulted), ei maint, a'r cenhedliad
  metadata y mae'n ei gario (angen `CAP_SYSINFO_HW`).
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
