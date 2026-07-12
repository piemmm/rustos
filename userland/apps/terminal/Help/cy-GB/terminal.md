## NAME

terminal — efelychydd terfynell graffigol

## SYNOPSIS

`terminal`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith sy'n cynnal cragen ragosodedig y
defnyddiwr ar sgrin 80×24 nod. Anfonir bysellau a deipir i'r ffenestr
â ffocws at y gragen; dehonglir popeth y mae'r gragen yn ei ysgrifennu
(yr allbwn safonol a'r gwall safonol fel ei gilydd) drwy'r eirfa
ANSI/VT a rennir a'i dynnu â phalet y thema weithredol. Nid yw'r
derfynell ei hun byth yn adleisio: mae adlais a golygu llinell yn
perthyn i'r gragen, yn union fel ar gonsol.

Lansir y derfynell o ddewislen cychwyn y bwrdd gwaith (y cofnod
`Terminal`) neu wrth ei henw o gragen. Mae angen sesiwn graffigol
weithredol arni: hebddi, mae sianel y ffenestr yn anghyraeddadwy ac
mae'r derfynell yn adrodd y gwrthodiad ar y ffrwd gwall safonol ac yn
gorffen.

Daw'r sesiwn i ben pan fydd y gragen yn gadael (er enghraifft gydag
`exit`) neu pan gaeir y ffenestr o'r bwrdd gwaith; mae cau'r ffenestr
yn gorffen y gragen gyda diwedd ffeil ar ei mewnbwn.

## EXIT STATUS

Sero ar ôl cau glân neu ymadawiad y gragen ei hun; heb fod yn sero pan
na ellid cynnal y gragen neu pan wrthodwyd sianel y ffenestr, y rhanbarth
fframiau a rennir neu'r blwch digwyddiadau (nodir y rheswm ar y ffrwd
gwall safonol).
