## NAME

stat — mostrar el estado de un archivo o de un sistema de archivos

## SYNOPSIS

`stat [-Lft] [-c FORMATO | --printf=FORMATO] [--] archivo...`

## DESCRIPTION

Muestra los campos de un estado leído por operando, en el orden de la
línea de órdenes.

**Sin `-L` un enlace simbólico se describe como él mismo**: para eso
sirve esta herramienta junto a `ls`. `%N` muestra el enlace y el destino
que almacena, `%F` dice `symbolic link`, y los tamaños y las marcas de
tiempo son los del propio enlace. `-L` resuelve el último enlace y
describe lo que nombra.

`-f` cambia al sistema de archivos que contiene el operando: los
recuentos de bloques e inodos del volumen, su tamaño de bloque y el tipo
que registra su montaje. Las dos lecturas tienen vocabularios de campos
**distintos**, así que un formato se comprueba contra el que `-f`
selecciona.

`-c`/`--format` muestra una cadena de formato por operando, seguida de un
salto de línea; `--printf` interpreta los escapes y no añade salto. Esa
es la única diferencia. Una directiva admite los indicadores y la anchura
de printf (`%-10s`, `%06i`, `%.3n`), para que un informe quede en
columnas. `-t` es la forma breve de una línea, en cualquiera de las dos
lecturas.

Un operando que no se puede leer se informa en la salida de error
estándar, los operandos restantes se siguen describiendo y la orden
termina con un estado distinto de cero. Un campo que este sistema no
puede facilitar —una instantánea de montajes que no puede leer, un uid
sin nombre en el directorio de usuarios— se muestra como `?` o como
`UNKNOWN`, nunca como un sustituto plausible.

Se requiere al menos un operando. `--` termina el análisis de opciones.

Cuatro campos nombran un concepto que TAIRiX no tiene y se **rechazan**
por su nombre cuando un formato usa uno, en vez de responderse con un
valor inventado: `%G`, porque la API de información del sistema publica
un directorio de usuarios y ninguno de grupos, de modo que `%g` (el
identificador numérico) es el campo honesto; `%t` y `%T` del vocabulario
de archivo, porque no hay archivos especiales de dispositivo que tengan
un tipo mayor o menor; y `%t` del vocabulario de sistema de archivos,
porque un volumen no tiene número mágico de tipo —`%T` nombra el tipo que
registra su montaje. El rechazo ocurre al analizar el formato, antes de
tocar ninguna ruta.

Dos campos informan de un concepto de TAIRiX en lugar de uno de Linux. Un
volumen se identifica por un id de 16 bytes y no por un número de
dispositivo, así que `%d` es ese id en decimal y `%D` en hexadecimal;
comparar el `%d` de dos archivos sigue respondiendo exactamente a «¿están
en un mismo volumen?».

## OPTIONS

- `-L, --dereference` — describir lo que nombra un enlace simbólico, en
  vez del enlace mismo.
- `-f, --file-system` — describir el sistema de archivos que contiene
  cada operando en vez del operando.
- `-c, --format=FORMAT` — mostrar `FORMATO` por operando, seguido de un
  salto de línea.
- `--printf=FORMAT` — como `-c`, pero interpretando los escapes y sin
  salto de línea final.
- `-t, --terse` — mostrar los campos en una sola línea separada por
  espacios.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `stat notas.txt` — el informe completo de un archivo.
- `stat -c '%s %n' *` — tamaño y nombre, una línea cada uno.
- `stat -L enlace` — describir lo que nombra el enlace, no el enlace.
- `stat -f .` — el volumen que contiene el directorio de trabajo.

## EXIT STATUS

- `0` — se describió cada operando (o se escribió la ayuda breve).
- `1` — al menos un operando no se pudo leer, o la salida falló.
- `2` — no se entendió la línea de órdenes, o su formato nombraba una
  directiva que este sistema no puede servir.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

ls, readlink, df, du
