## NAME

wc — imprimir el número de líneas, palabras y bytes de cada fichero

## SYNOPSIS

`wc [option...] [file...]`

`wc [option...] --files0-from <file>`

## DESCRIPTION

Cuenta, para cada `file`, sus líneas (caracteres de salto de línea),
palabras y bytes, y los imprime en una fila seguida del nombre del
fichero. Sin `file`, o cuando `file` es `-`, se lee la entrada estándar
(y no se imprime nombre para la forma sin operandos). Con más de una
entrada, se imprime una fila final `total` según `--total`.

Los selectores `-l`, `-w`, `-m`, `-c` y `-L` eligen qué recuentos se
imprimen; sin ninguno, se imprimen los recuentos de líneas, palabras y
bytes. Los recuentos aparecen siempre en el orden fijo: líneas,
palabras, caracteres, bytes, anchura máxima de línea. Una palabra es
una secuencia máxima de caracteres que no son espacios en blanco. `-m`
cuenta caracteres UTF-8 (un byte que no es UTF-8 válido cuenta como
byte pero no como carácter); `-L` mide la anchura de visualización de
cada línea en columnas de terminal, con los tabuladores avanzando al
siguiente múltiplo de 8.

`--files0-from <file>` lee la lista de operandos, separados por NUL,
desde `file` (`-` significa la entrada estándar); no puede combinarse
con operandos `file`.

Una entrada ilegible se informa en la salida de error y la ejecución
continúa con la entrada siguiente.

## OPTIONS

- `-c, --bytes` — imprimir el número de bytes.
- `-m, --chars` — imprimir el número de caracteres.
- `-l, --lines` — imprimir el número de saltos de línea.
- `-w, --words` — imprimir el número de palabras.
- `-L, --max-line-length` — imprimir la anchura de visualización
  máxima de una línea.
- `--files0-from <file>` — leer la lista de operandos separados por
  NUL desde `file` (`-` la lee desde la entrada estándar).
- `--total <when>` — cuándo imprimir la fila `total`: `auto` (por
  defecto: solo con más de una entrada), `always`, `only` (solo el
  total, sin etiqueta) o `never`.
- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `wc notes.txt` — imprimir los recuentos de líneas, palabras y bytes
  de `notes.txt`.
- `wc -l a b` — imprimir el número de líneas de `a` y de `b`, y luego
  el total.
- `wc -L table.txt` — imprimir la línea más ancha de `table.txt` en
  columnas de terminal.
- `wc -c --total=only a b` — imprimir solo la suma de bytes.

## EXIT STATUS

- `0` — se contó cada entrada (o se escribió la ayuda breve).
- `1` — no se pudo leer una entrada, o no se pudo entregar la salida.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `cat`
- `head`
- `man`
