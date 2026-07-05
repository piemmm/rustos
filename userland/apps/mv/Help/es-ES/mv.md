## NAME

mv — mover (renombrar) archivos y directorios

## SYNOPSIS

`mv [-finvT] [-t dir] [--] source... dest`

## DESCRIPTION

Mueve cada operando de origen a un destino. Con un único origen y un
destino que no nombra un directorio, el origen se renombra a esa ruta
exacta. Cuando el destino nombra un directorio existente — y siempre
que haya más de un origen — cada origen se mueve *dentro* de ese
directorio bajo su propio nombre base.

Un movimiento dentro de un mismo volumen es un renombrado atómico que
conserva la identidad del nodo. Un movimiento cuyo origen y destino
están en volúmenes distintos no puede ser atómico: recurre a copiar
el origen al destino y luego eliminar el origen (los directorios se
reproducen recursivamente).

Un destino existente se sobrescribe por defecto, se omite con `-n` y
se pregunta por la salida de error estándar con `-i` (una pregunta
rechazada omite ese movimiento sin error; una respuesta ilegible
nunca se considera consentimiento). El primer fallo detiene la
ejecución antes de cualquier operando posterior. `--` termina el
análisis de opciones: cada argumento posterior es una ruta.

## OPTIONS

- `-f, --force` — eliminar un destino que bloquea y reintentar el
  renombrado; no preguntar nunca. El último de `-f`, `-i` y `-n`
  gana.
- `-i, --interactive` — preguntar antes de sobrescribir un destino
  existente; solo consiente una respuesta que empiece por `y`/`Y`.
- `-n, --no-clobber` — no sobrescribir nunca un destino existente.
- `-v, --verbose` — informar de cada movimiento como
  `renamed 'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — mover cada origen dentro de
  `dir`, que debe ser un directorio existente. El valor sigue adjunto
  (`-tdir`, `--target-directory=dir`) o como argumento siguiente.
- `-T, --no-target-directory` — tratar el destino como un archivo
  normal; se permite exactamente un origen. No puede combinarse con
  `-t`.
- `-h, -?, --help` — mostrar la ayuda corta de esta orden.

## EXAMPLES

- `mv draft.txt final.txt` — renombrar un archivo.
- `mv -v a.txt b.txt Archive` — mover ambos archivos a `Archive`,
  informando de cada movimiento.
- `mv -n new.cfg current.cfg` — instalar un archivo solo si el
  destino no existe ya.

## EXIT STATUS

- `0` — todos los movimientos tuvieron éxito (una omisión por `-n` y
  una pregunta `-i` rechazada no son fallos).
- `1` — un fallo del sistema de archivos, de la pregunta o de la
  salida; el motivo se imprime en la salida de error estándar.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `cp`
- `ls`
- `rm`
