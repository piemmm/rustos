## NAME

unlink — tynnu un enw

## SYNOPSIS

`unlink [--] ffeil`

## DESCRIPTION

Yn tynnu un enw yn union, drwy'r un galwad system ffeiliau a enwir gan y
ffwythiant POSIX `unlink`. Yn fwriadol nid oes ymrediad, gorfodi, gofyn
na gwaith adrodd: mae sgript sy'n gorfod tynnu un enw a dim arall yn cael
offeryn na all wneud mwy. Defnyddiwch `rm` am y dewisiadau hynny a
`rmdir` am gyfeiriadur.

Tynnir yr enw **fel y'i teipiwyd**. Tynnir cyswllt symbolaidd ei hun ac
ni ddilynir ef erioed, felly ni all cyswllt a blannwyd yno ailgyfeirio'r
tynnu at ei darged.

Gwrthodir **cyfeiriadur** gan y system ffeiliau, yn yr un daith dan glo a
fyddai wedi tynnu'r cofnod — nid oes ras rhwng gwirio a thynnu yma.

Mae angen un operand yn union: dim operand a dau operand neu fwy — mae'r
ddau yn wallau defnydd, ac ni thynnir dim. Mae `--` yn dod â dadansoddi
dewisiadau i ben, felly mae enw sy'n dechrau â chysylltnod yn parhau'n
dynadwy.

## OPTIONS

- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `unlink hen.log` — tynnu un enw.
- `unlink Home:/Documents/alias` — tynnu'r cyswllt symbolaidd ei hun,
  nid yr hyn y mae'n cyfeirio ato.
- `unlink -- -enw-rhyfedd` — tynnu enw sy'n dechrau â chysylltnod.

## EXIT STATUS

- `0` — tynnwyd yr enw (neu ysgrifennwyd y cymorth byr).
- `1` — gwrthododd y system ffeiliau'r tynnu, neu methodd yr allbwn;
  argreffir y rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

rm, rmdir, ln, link, readlink
