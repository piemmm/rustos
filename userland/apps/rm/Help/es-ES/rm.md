## NAME

rm — eliminar archivos y directorios

## SYNOPSIS

`rm [-dfiIrRv] [--] file...`

## DESCRIPTION

Elimina cada operando de archivo, en orden. Un operando que no es un
directorio se desenlaza; un operando de directorio solo se elimina
con `-r` (que elimina su contenido en profundidad primero y luego el
directorio mismo) o, cuando está vacío, con `-d`.

Con `-f`, un operando que no existe se omite en silencio y nunca se
hace una pregunta. `-i` pregunta por la salida de error estándar
antes de cada eliminación y antes de descender a un directorio; `-I`
pregunta una sola vez antes de eliminar más de tres operandos o antes
de una eliminación recursiva. Una pregunta rechazada omite el objeto
(o toda la ejecución, con `-I`) sin error; una respuesta ilegible
nunca se considera consentimiento. El último de `-f`, `-i` e `-I`
gana.

El operando `/` se rechaza bajo `--preserve-root`, el comportamiento
predeterminado. El primer fallo detiene la ejecución antes de
cualquier operando posterior. `--` termina el análisis de opciones:
cada argumento posterior es una ruta.

## OPTIONS

- `-r, -R, --recursive` — eliminar directorios y su contenido.
- `-f, --force` — ignorar los operandos que no existen; no preguntar
  nunca.
- `-d, --dir` — eliminar directorios vacíos.
- `-i, --interactive` — preguntar antes de cada eliminación; solo
  consiente una respuesta que empiece por `y`/`Y`.
- `-I` — preguntar una sola vez antes de eliminar más de tres
  operandos, o antes de una eliminación recursiva.
- `-v, --verbose` — informar de cada eliminación como
  `removed 'file'`.
- `--preserve-root` — negarse a eliminar `/` (el comportamiento
  predeterminado).
- `--no-preserve-root` — permitir eliminar `/`.
- `-h, -?, --help` — mostrar la ayuda corta de esta orden.

## EXAMPLES

- `rm notes.txt` — eliminar un archivo.
- `rm -r Scratch` — eliminar el árbol `Scratch` y todo su contenido.
- `rm -I a b c d` — preguntar una vez y, con `y`, eliminar los cuatro
  archivos.

## EXIT STATUS

- `0` — todas las eliminaciones tuvieron éxito (una pregunta
  rechazada y una omisión por `-f` no son fallos).
- `1` — un fallo del sistema de archivos, de la pregunta o de la
  salida; el motivo se imprime en la salida de error estándar.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `cp`
- `ls`
- `mv`
