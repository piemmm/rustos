## NAME

chmod — newid didau modd ffeil

## SYNOPSIS

`chmod [-cfRv] [--] MODE file...`

## DESCRIPTION

Yn newid didau caniatâd pob operand ffeil i `MODE`, yn eu trefn. Mae
`MODE` naill ai'n werth wythol absoliwt (`644`, `0755`, …) sy'n
disodli'r didau caniatâd yn llwyr, neu'n rhestr o gymalau symbolaidd
wedi'u gwahanu gan atalnodau `[ugoa]*[-+=][rwxXst]*` (`g+w`, `o-rx`,
`a=rx`, `u+s`) sy'n trawsffurfio didau presennol y ffeil. Dim ond i
gyfeiriadur, neu i ffeil sydd eisoes yn cario did gweithredu, y mae'r
`X` symbolaidd yn rhoi gweithredu.

Dim ond perchennog ffeil all newid ei modd; mae'r cnewyllyn yn
gwrthod unrhyw un arall, ac nid yw dal capability yn rhoi unrhyw
oruchafiaeth. Gyda `-R` caiff operand cyfeiriadur ei newid ac yna
caiff ei gynnwys ei newid yn ailadroddus. Mae'r methiant cyntaf yn
atal y rhediad cyn unrhyw operand diweddarach. Mae `--` yn gorffen
dosrannu opsiynau: mae pob dadl ddiweddarach yn operand. Ar gyfer modd
sy'n dechrau gyda `-`, ysgrifennwch ef heb y llinell doriad (`a-w`)
neu gorffennwch yr opsiynau'n gyntaf (`chmod -- -w file`).

## OPTIONS

- `-R, --recursive` — newid ffeiliau a chyfeiriaduron yn
  ailadroddus.
- `-c, --changes` — adrodd dim ond y ffeiliau y newidiodd eu modd
  mewn gwirionedd.
- `-v, --verbose` — adrodd pob ffeil a broseswyd.
- `-f, --silent, --quiet` — atal y rhan fwyaf o negeseuon gwall;
  mae'r rhediad yn dal i fethu ac mae'r statws gadael yn ei adrodd.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `chmod 644 notes.txt` — darllen/ysgrifennu i'r perchennog,
  darllen yn unig i bawb arall.
- `chmod g+w shared.txt` — ychwanegu ysgrifennu grŵp at y didau
  presennol.
- `chmod -R a=rx Docs` — gwneud y goeden `Docs` yn ddarllenadwy ac
  yn dramwyadwy i bawb.

## EXIT STATUS

- `0` — llwyddodd pob newid modd.
- `1` — methiant system ffeiliau neu allbwn; caiff y rheswm ei
  argraffu ar yr allbwn gwall (wedi'i atal o dan `-f`).
- `2` — ni ddeallwyd y llinell orchymyn, neu nid oedd yr operand
  modd yn wythol nac yn symbolaidd.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47
  megis `cy-GB`).

## SEE ALSO

- `ls`
- `mkdir`
- `rm`
