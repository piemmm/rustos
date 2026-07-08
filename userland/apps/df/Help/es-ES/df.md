## NAME

df — informar del uso de espacio de los sistemas de archivos

## SYNOPSIS

`df [option...] [file...]`

## DESCRIPTION

Informa, una fila por sistema de archivos montado, del tamaño del
volumen, el espacio usado, el espacio disponible, el porcentaje usado
y el punto de montaje. Con operandos `file` informa en su lugar del
sistema de archivos que contiene cada operando (una fila por sistema
de archivos, cubra los operandos que cubra).

Las cifras proceden del listado de montajes de la API de información
del sistema, tal como cada controlador de sistema de archivos montado
informa de su propia contabilidad. De forma predeterminada, el informe
oculta los montajes sin capacidad propia (los enlaces de vista
sintéticos del sistema) y los montajes adicionales de un volumen ya
listado; `-a` lo muestra todo, y el número de entradas ocultas se
anota en el flujo de información estándar (fd 3), nunca en la tabla.

Los tamaños se imprimen en bloques de 1024 bytes salvo que una opción
de unidad elija otra cosa; una opción de unidad posterior sustituye a
la anterior, y los recuentos de bloques se redondean hacia arriba. Un
sistema de archivos cuyo formato asigna inodos bajo demanda informa de
cifras de inodos a cero con `-i` — la respuesta honesta «sin
seguimiento».

Un operando `file` que no existe, o que es una ruta relativa (los
puntos de montaje son absolutos; `df` nunca adivina una resolución),
se informa en la salida de error estándar y el informe continúa con el
resto. Las opciones GNU `--output`, `--sync` y `--no-sync` aún no
están disponibles.

## OPTIONS

- `-a, --all` — incluir los montajes sin capacidad y duplicados que
  el comportamiento predeterminado oculta.
- `-T, --print-type` — añadir la columna del tipo de sistema de
  archivos.
- `-t, --type <type>` — informar solo de los sistemas de archivos del
  tipo `type` (repetible).
- `-x, --exclude-type <type>` — omitir los sistemas de archivos del
  tipo `type` (repetible).
- `-i, --inodes` — informar de recuentos de inodos en lugar del uso
  de bloques.
- `-P, --portability` — el formato portable POSIX (cabeceras
  `1024-blocks` y `Capacity`).
- `-l, --local` — restringir el informe a sistemas de archivos
  locales (todo montaje de RustOS hoy: no se filtra nada).
- `--total` — añadir una fila etiquetada `total` que suma las cifras
  mostradas.
- `-k` — bloques de 1024 bytes (el valor predeterminado).
- `-h, --human-readable` — tamaños legibles en potencias de 1024
  (`1.0K`, `23M`).
- `-H, --si` — tamaños legibles en potencias de 1000 (`1.0k`, `23M`).
- `-B, --block-size <size>` — informar en bloques de `size` bytes
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `df` — el uso de cada volumen real en bloques de 1024 bytes.
- `df -h` — lo mismo, en tamaños legibles.
- `df /Users/jo` — el sistema de archivos que contiene `/Users/jo`.
- `df -aT` — cada montaje, con su tipo de sistema de archivos.
- `df --total -k` — los volúmenes más una fila `total` sumada.

## EXIT STATUS

- `0` — el informe cubrió todo lo pedido (o se escribió la ayuda
  breve).
- `1` — no se pudo informar de un operando, los filtros no dejaron
  nada, o falló la consulta o la salida.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — el idioma preferido para la ayuda breve (una etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

- `du`
- `mount`
- `man`
