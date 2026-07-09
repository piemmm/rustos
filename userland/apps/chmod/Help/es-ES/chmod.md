## NAME

chmod — cambiar los bits de modo de un archivo

## SYNOPSIS

`chmod [-cfRv] [--] MODE file...`

## DESCRIPTION

Cambia los bits de permiso de cada operando de archivo a `MODE`, en
orden. `MODE` es o bien un valor octal absoluto (`644`, `0755`, …)
que sustituye por completo los bits de permiso, o bien una lista de
cláusulas simbólicas separadas por comas `[ugoa]*[-+=][rwxXst]*`
(`g+w`, `o-rx`, `a=rx`, `u+s`) que transforman los bits actuales del
archivo. La `X` simbólica concede ejecución solo a un directorio o a
un archivo que ya lleve un bit de ejecución.

Solo el propietario de un archivo puede cambiar su modo; el núcleo
rechaza a cualquier otro, y poseer una capability no concede ningún
privilegio. Con `-R` un operando de directorio se cambia y después su
contenido se cambia recursivamente. El primer fallo detiene la
ejecución antes de cualquier operando posterior. `--` termina el
análisis de opciones: cada argumento posterior es un operando. Para
un modo que empiece por `-`, escríbalo sin el guion (`a-w`) o termine
antes las opciones (`chmod -- -w file`).

## OPTIONS

- `-R, --recursive` — cambiar archivos y directorios
  recursivamente.
- `-c, --changes` — informar solo de los archivos cuyo modo cambió
  realmente.
- `-v, --verbose` — informar de cada archivo procesado.
- `-f, --silent, --quiet` — suprimir la mayoría de los mensajes de
  error; la ejecución sigue fallando y el estado de salida lo
  refleja.
- `-h, -?, --help` — mostrar la ayuda corta de esta orden.

## EXAMPLES

- `chmod 644 notes.txt` — lectura/escritura para el propietario,
  solo lectura para los demás.
- `chmod g+w shared.txt` — añadir escritura de grupo a los bits
  actuales.
- `chmod -R a=rx Docs` — hacer el árbol `Docs` legible y
  atravesable por todos.

## EXIT STATUS

- `0` — todos los cambios de modo tuvieron éxito.
- `1` — un fallo del sistema de archivos o de la salida; la razón se
  imprime en la salida de error (suprimida bajo `-f`).
- `2` — la línea de órdenes no se entendió, o el operando de modo no
  era ni octal ni simbólico.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `ls`
- `mkdir`
- `rm`
