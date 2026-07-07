## NAME

mkdir — creu cyfeiriaduron

## SYNOPSIS

`mkdir [-pv] [--] directory...`

## DESCRIPTION

Mae'n creu pob operand cyfeiriadur, yn eu trefn. Heb `-p` rhaid i
riant pob operand fodoli eisoes ac ni chaiff yr operand ei hun fodoli;
mae'r methiant cyntaf yn atal y rhediad cyn unrhyw operand
diweddarach.

Gydag `-p` crëir pob hynafiad coll yn gyntaf, y mwyaf allanol yn
gyntaf, ac nid yw operand (neu hynafiad) sydd eisoes yn bodoli fel
cyfeiriadur yn wall. Mae hynafiad sy'n bodoli fel ffeil yn dal i
fethu: ni ddisodlir dim byth yn dawel.

Nid yw `-m`/`--mode` `mkdir` GNU yn cael ei dderbyn eto: crëir
cyfeiriaduron â modd rhagosodedig y system ffeiliau nes i'r cyfleuster
gosod moddau lanio, a daw'r switsh gydag ef yn hytrach na chael ei
anwybyddu. Mae `--` yn gorffen dosrannu opsiynau: mae pob ymresymiad
diweddarach yn llwybr.

## OPTIONS

- `-p, --parents` — creu cyfeiriaduron rhiant coll; nid yw operand
  sydd eisoes yn gyfeiriadur yn wall.
- `-v, --verbose` — adrodd am bob cyfeiriadur a grëwyd fel
  `mkdir: created directory 'dir'`.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun (hefyd
  `--help`).

## EXAMPLES

- `mkdir Notes` — creu un cyfeiriadur yn y cyfeiriadur cyfredol.
- `mkdir -p Projects/os/build` — creu'r gadwyn gyfan, gan hepgor y
  rhannau sydd eisoes yn bodoli.
- `mkdir -pv Home:/tools/bin` — creu o dan wreiddyn alias, gan adrodd
  am bob cyfeiriadur newydd.

## EXIT STATUS

- `0` — crëwyd pob cyfeiriadur (neu, o dan `-p`, roedd eisoes yn
  bodoli).
- `1` — methiant system ffeiliau neu allbwn; argraffir y rheswm ar y
  gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

rmdir, rm, ls
