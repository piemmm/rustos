## NAME

wallpaper — dewiswr cefndir bwrdd gwaith graffigol

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith sy'n cynnig y papurau wal y mae'r system
yn eu darparu, y lliw cefndir y tu ôl iddynt, a sut mae'r bwrdd gwaith
yn trefnu'r eiconau ar ei fwrdd pinio. Nid oes dim yn newid ar y sgrin
nes bod y gosodiadau'n cael eu cymhwyso.

Mae'r ffenestr yn cael ei gyrru gan y llygoden. Mae rhagolwg mawr ar y
top yn dangos y papur wal a ddewiswyd fel y bydd y bwrdd gwaith yn ei
dynnu, gyda'r lliw cefndir a ddewiswyd lle bynnag nad yw'r ddelwedd yn
cyrraedd. O dano, mae'r oriel yn rhestru pob papur wal a ddarperir fel
teilsen: cliciwch ar un i'w ddewis ac mae'r rhagolwg yn dilyn ar unwaith.
Mae'r deilsen **No wallpaper** (Dim papur wal), bob amser yn gyntaf, yn
dangos y lliw cefndir a ddewiswyd yn unig.

Mae'r oriel yn sgrolio pan fydd yn cynnwys mwy o deils nag y mae'r
ffenestr yn eu dangos. Trowch y rhod mewn unrhyw le dros y ffenestr,
llusgwch fawd y bar sgrolio ar yr ymyl ôl, neu cliciwch y trac uwchben
neu o dan y bawd i symud un dudalen ar y tro.

Wrth ymyl y rhagolwg mae pedwar gosodiad, pob un yn rhestr ollwng.
Cliciwch ar un i'w agor a cliciwch ar ddewis i'w gymryd:

- **Fit** (Ffitio) — sut mae'r ddelwedd wedi'i gosod: `fill` (llenwi'r
  sgrin, gan docio'r gorlif), `fit` (ei chynnwys yn gyfan, lliw cefndir
  yn y bariau), `stretch` (ystumio i union faint y sgrin), `centre`
  (maint brodorol, wedi'i ganoli), a `tile` (ailadrodd o'r top chwith).
- **Backdrop** (Cefndir) — y lliw gwastad a ddangosir lle bynnag nad yw'r
  papur wal yn cyrraedd: mae `Theme` yn dilyn thema weithredol y bwrdd
  gwaith, ac mae'r lliwiau a enwir yn sefydlog. Cynigir lliw sydd eisoes
  mewn grym nad yw'n un o'r rhai a enwir o dan ei sillafu `rrggbb` ei
  hunan.
- **Icons** (Eiconau) — cornel y bwrdd pinio y mae grid eiconau'r bwrdd
  gwaith yn tyfu ohono.
- **Sort** (Trefnu) — y drefn y rhestrir eiconau ffolder y bwrdd gwaith.

Mae'r rhagolwg yn dangos y ddelwedd, y cefndir a'r ffit a ddewiswyd yn
siâp y rhagolwg ei hun. Mae sgrin o siâp gwahanol yn tocio neu'n ychwanegu
bariau'n wahanol, felly mae'r rhagolwg yn olygfa ffyddlon o'r llun ac o'r
rheol ffitio, nid model graddfa o'r ardangosydd.

Nid yw'r rhaglen hon byth yn dadgodio delweddau papur wal. Mae pob un
yn cael ei rendro gan broses focs tywod ar wahân nad oes ganddi awdurdod
ar y system ffeiliau, y rhwydwaith na lansio prosesau, felly ni all
delwedd gamffurfiedig beryglu'r dewiswr na'r bwrdd gwaith. Mae ffeil na
ellir ei dadgodio wedi'i marcio yn `unreadable` yn ei theilsen ac ni
cheisir ei hagor eto.

Mae'r fysellfwrdd yn cyrraedd popeth y mae'r llygoden yn ei wneud. Mae
`Tab` a `Shift-Tab` yn symud y ffocws ymlaen ac yn ôl trwy'r oriel, y
pedwar gosodiad, a'r ddau fotwm. Mae'r bysellau saeth yn symud o fewn yr
oriel, neu'n agor rhestr y gosodiad sydd â ffocws ac yn symud ynddi. Mae
`Enter` yn cymhwyso, neu'n actifadu'r botwm sydd â ffocws, ac mae
`Escape` yn cau'r ffenestr heb gymhwyso.

Mae cymhwyso yn anfon y gosodiadau a ddewiswyd i'r sesiwn bwrdd gwaith,
sy'n penderfynu a ddylid eu mabwysiadu, yn ail-dynnu'r bwrdd pinio, ac
yn eu cadw ar gyfer y mewngofnodi nesaf. Nid yw'r rhaglen hon byth yn
ysgrifennu'r gosodiadau ei hun. Adroddir y canlyniad wrth ymyl y
botymau: wedi'i gymhwyso, wedi'i wrthod gyda rheswm y sesiwn, neu ddim
sesiwn bwrdd gwaith yn gwrando. Mae gwrthodiad yn gadael y ffenestr yn
agored gyda'r dewis yn gyfan.

Dim ond y siop papurau wal a ddarperir sy'n cael ei chynnig; ni ellir
dewis delwedd mewn man arall ar y system o'r ffenestr hon.

## EXIT STATUS

Sero ar ôl cau'n lân, gan gynnwys pan wrthodwyd y gosodiadau. Nid yw'n
sero pan na ellid agor y ffenestr, pan wrthodwyd y rhanbarth ffrâm a
rannwyd, neu pan gollwyd y sianel ffenestr; nodir y rheswm ar y ffrwd
gwallau safonol.

## ENVIRONMENT

Mae `HOME` yn enwi cyfeiriadur cartref y defnyddiwr, y darperir
`Settings/Pinboard/pinboard.conf` oddi tano wrth gychwyn fel bod y
ffenestr yn agor ar y gosodiadau sydd mewn grym. Mae'r ddogfen honno
wedi'i hysgrifennu gan y sesiwn bwrdd gwaith, byth gan y rhaglen hon. Heb
`HOME`, mae'r ffenestr yn agor ar y rhagosodiadau.

## SEE ALSO

`files`, `viewer`
