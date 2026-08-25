## NAME

ss — rhestru'r socedau agored

## SYNOPSIS

`ss [option...]`

## DESCRIPTION

Rhestru socedau agored y system, un rhes fesul soced: y protocol
cludiant, cyflwr y cysylltiad, dyfnder y ciwiau derbyn ac anfon, yr
`address:port` lleol a phell, a — gyda `-p` — y broses berchennog.

Daw'r rhesi o restr socedau'r API Gwybodaeth System, y mae'r pentwr
rhwydwaith yn ei hateb fel ymholiad breintiedig ac archwiliedig: mae'n
enwi socedau pob prifathro a chymar pob cysylltiad, felly mae rhestru
pob soced yn gofyn am `CAP_SYSINFO_GLOBAL`. Nid oes `/proc/net`;
dywedir wrth sesiwn heb y gallu hwnnw ac mae `ss` yn gorffen, yn hytrach
nag argraffu tabl gwag.

Yn ddiofyn mae'r rhestr yn dangos socedau cysylltiedig, nad ydynt yn
gwrando. Mae `-l` yn dangos socedau sy'n gwrando yn unig ac `-a` y ddau;
nodir nifer y gwrandawyr cudd ar y ffrwd wybodaeth safonol (fd 3), byth
yn y tabl. Mae `-t` ac `-u` yn cyfyngu'r protocol ac `-4`/`-6` deulu'r
cyfeiriadau; heb yr un, dangosir pob protocol a theulu. Mae'r pyrth bob
amser yn rhifol (nid oes gan TAIRiX gronfa enwau gwasanaeth), felly
derbynnir `-n` ond mae bob amser mewn grym arnynt. Mae'r cyfeiriadau'n
rhifol hefyd oni bai bod `-r` yn gofyn am enwau gwesteiwyr: mae `-r` yn
datrys pob un drwy ddatryswr y system (ymholiad `PTR`), yn ymholi pob
cyfeiriad gwahanol unwaith, ac yn gadael cyfeiriad heb enw yn rhifol.
Argreffir cyfeiriad heb ei bennu fel `*` a phorth heb ei rwymo fel `*`;
rhoddir cyfeiriad IPv6 mewn cromfachau sgwâr fel bod y gwahanydd `:port`
yn aros yn ddiamwys — nid oes angen cromfachau ar enw a ddatryswyd.

Dim ond opsiynau y mae `ss` yn eu derbyn. Nid yw gramadeg mynegiadau
hidlo iproute2 (hidlau cyflwr a chyfeiriad) wedi'i weithredu, felly mae
operand noeth yn wall defnydd yn hytrach nag ymresymiad a anwybyddir yn
dawel.

## OPTIONS

- `-t, --tcp` — dangos socedau TCP. Heb `-t` nac `-u`, dangosir y ddau
  brotocol.
- `-u, --udp` — dangos socedau UDP.
- `-a, --all` — dangos socedau sy'n gwrando a rhai cysylltiedig.
- `-l, --listening` — dangos socedau sy'n gwrando yn unig.
- `-n, --numeric` — peidio â datrys enwau gwasanaeth. Bob amser mewn
  grym ar TAIRiX; derbynnir er cyfarwydd-dra. Mater i `-r` yw enwau
  gwesteiwyr.
- `-r, --resolve` — datrys cyfeiriadau'n enwau gwesteiwyr dros DNS. I
  ffwrdd yn ddiofyn, felly nid yw'r rhestr yn anfon ymholiad heb ofyn.
- `-p, --processes` — ychwanegu colofn y broses berchennog (`pid=N`).
- `-4, --ipv4` — cyfyngu'r rhestr i socedau IPv4.
- `-6, --ipv6` — cyfyngu'r rhestr i socedau IPv6.
- `-H, --no-header` — atal y rhes bennawd.
- `-s, --summary` — argraffu cyfansymiau amddiffyn cysylltiadau TCP
  y pentwr yn lle'r tabl socedi.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `ss` — y socedau cysylltiedig, nad ydynt yn gwrando.
- `ss -a` — pob soced, yn gwrando ac yn gysylltiedig.
- `ss -l` — y socedau sy'n gwrando yn unig.
- `ss -tlp` — socedau TCP sy'n gwrando, gyda'r broses berchennog.
- `ss -u4` — y socedau UDP dros IPv4.
- `ss -r` — yr un rhestr gyda'r cyfeiriadau wedi'u datrys yn enwau.

## EXIT STATUS

- `0` — cynhyrchwyd y rhestr (neu ysgrifennwyd y cymorth byr).
- `1` — gwrthodwyd yr ymholiad socedau neu methodd, neu ni ellid
  ysgrifennu'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 megis
  `fr-FR`).

## SEE ALSO

- `ping`
- `sysinfo`
- `man`
