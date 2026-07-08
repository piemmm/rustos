## NAME

du — estimar el espacio en disco usado por los archivos

## SYNOPSIS

`du [option...] [file...]`

## DESCRIPTION

Recorre cada operando `file` e imprime, por directorio (el más
profundo primero), el almacenamiento que ocupa el árbol situado
debajo, como `size<TAB>path`. Sin `file` se recorre el directorio
actual (`.`). Un operando `file` que no sea un directorio se imprime
por sí solo.

La medida predeterminada es el almacenamiento realmente asignado de
cada nodo, tal como lo informa el sistema de archivos montado; los
archivos dispersos o comprimidos cuentan lo que realmente ocupan.
`--apparent-size` (o `-b`) mide en su lugar las longitudes aparentes
en bytes. Los tamaños se imprimen en bloques de 1024 bytes salvo que
una opción de unidad elija otra cosa; una opción de unidad posterior
sustituye a la anterior, y los recuentos de bloques se redondean
hacia arriba (un bloque parcialmente usado es un bloque usado).

Una ruta ilegible se informa en la salida de error estándar y el
recorrido continúa con lo que queda; un directorio ilegible no aporta
nada en lugar de una suma parcial adivinada.

RustOS aún no tiene enlaces duros, así que ninguna entrada puede
contarse dos veces y los conmutadores GNU de deduplicación de enlaces
no existen; `-x` (un solo sistema de archivos) aún no está
disponible; las variables de entorno de la familia `DU_BLOCK_SIZE` no
se leen — la escala se elige solo mediante opciones.

## OPTIONS

- `-a, --all` — informar también de cada archivo, no solo de los
  directorios.
- `-s, --summarize` — informar solo del total de cada operando (en
  conflicto con `-a` y `-d`).
- `-c, --total` — añadir una fila de total general etiquetada
  `total`.
- `-d, --max-depth <n>` — informar de directorios hasta `n` niveles
  bajo un operando (`0` informa solo de los operandos); los totales
  no cambian.
- `-S, --separate-dirs` — la fila de un directorio excluye sus
  subdirectorios.
- `--apparent-size` — medir longitudes aparentes en bytes, no el
  almacenamiento asignado.
- `-b, --bytes` — tamaño aparente en bytes sueltos
  (`--apparent-size` con un tamaño de bloque de 1).
- `-k` — bloques de 1024 bytes (el valor predeterminado).
- `-m` — bloques de 1048576 bytes.
- `-h, --human-readable` — tamaños legibles en potencias de 1024
  (`1.0K`, `23M`).
- `--si` — tamaños legibles en potencias de 1000 (`1.0k`, `23M`).
- `-B, --block-size <size>` — informar en bloques de `size` bytes
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-0, --null` — terminar cada fila con NUL en lugar de salto de
  línea.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `du` — el árbol del directorio actual, una fila por directorio.
- `du -sh /Users/jo` — un total legible para `/Users/jo`.
- `du -a docs` — cada archivo y directorio bajo `docs`.
- `du -d1 -c /Apps /Users` — el primer nivel de cada almacén y luego
  un total general.

## EXIT STATUS

- `0` — todos los operandos se recorrieron (o se escribió la ayuda
  breve).
- `1` — no se pudo leer una ruta o no se pudo entregar la salida.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — el idioma preferido para la ayuda breve (una etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

- `df`
- `ls`
- `man`
