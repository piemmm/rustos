## NAME

datetime — gosod dyddiad ac amser y peiriant

## SYNOPSIS

`datetime`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith sy'n dangos cloc y peiriant yn chwe maes
golygadwy — blwyddyn, mis a diwrnod ar y rhes gyntaf, awr, munud ac eiliad
ar yr ail — ac yn gosod y cloc i'r hyn a nodir ynddynt. Nid yw dim yn
newid hyd nes pwyso **Set**.

UTC yw'r darlleniad. Nid yw TAIRiX yn cadw unrhyw wrthbwyso cylchfa
amser, felly nid oes amser lleol i'w ddangos nac i'w rhoi.

Fel arfer cyrhaeddir y ffenestr o fwydlen cloc y bwrdd gwaith ei hun:
cliciwch y cloc yn y bar eiconau a dewis **Set Date & Time…**. Mae gosod
y cloc yn galw am awdurdod nad yw sesiwn bwrdd gwaith yn ei feddu, felly
mae'r bwrdd gwaith yn gofyn am gyfrif sy'n ei feddu, ac fe gychwynnir y
rhaglen hon fel y cyfrif hwnnw wedi derbyn y cyfrinair.

Cliciwch faes i deipio ynddo, neu pwyswch `Tab` i symud i'r nesaf. Dim
ond digidau a dderbynnir, gyda `-` arweiniol yn cael ei ganiatáu yn y
flwyddyn ar gyfer dyddiad cyn blwyddyn 1. Mae `Enter` yn gosod y cloc;
mae `Escape` yn cau'r ffenestr.

Gwirir pob maes cyn gosod dim, a nodir y diffyg cyntaf yn y ffenestr yn
lle ei gywiro'n dawel: mis y tu allan i 1 i 12, awr y tu allan i 0 i 23,
munud neu eiliad y tu allan i 0 i 59, neu ddiwrnod nad yw'n bod yn y mis
a'r flwyddyn a roddwyd — 31 Ebrill, neu 29 Chwefror y tu allan i flwyddyn
naid. Ni osodir dim pan wrthodir maes.

Mae dyddiadau cyn 1970 ac ymhell ar ôl 2038 yn gofnodion cyffredin. Gwerth
64-did arwyddedig yw'r cloc, felly nid yw'r naill na'r llall yn derfyn.

Os na osodwyd cloc y peiriant erioed er iddo gychwyn, mae'r meysydd yn
agor yn **wag** ac mae'r ffenestr yn dweud hynny. Nid ydynt yn cael eu
llenwi ag epoc Unix, a fyddai'n ddyddiad na honnodd y peiriant erioed.

Os nad yw'r cyfrif y mae'r rhaglen hon yn rhedeg fel ef yn cael gosod y
cloc, gwrthodir y cynnig, mae'r ffenestr yn dweud hynny, ac erys y cloc
yn union fel yr oedd. Ysgrifennir y rheswm hefyd i'r ffrwd gwallau safonol.
Mae'r rhaglen yn dal i redeg: ateb yw gosodiad a wrthodwyd, nid nam ar y
rhaglen.

## EXIT STATUS

Sero ar ôl cau'n lân, gan gynnwys pan wrthodwyd gosodiad. Nid sero pan na
allwyd agor y ffenestr, pan wrthodwyd y rhanbarth ffrâm a rennir, neu pan
gollwyd sianel y ffenestr; nodir y rheswm ar y ffrwd gwallau safonol.

## SEE ALSO

`sysinfo`, `uptime`
