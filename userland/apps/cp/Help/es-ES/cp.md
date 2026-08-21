## NAME

cp — copiar archivos y directorios

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

Copia cada operando de origen a un destino. Con un único origen y un
destino que no nombra un directorio, el origen se copia a esa ruta
exacta. Cuando el destino nombra un directorio existente — y siempre
que haya más de un origen — cada origen se copia *dentro* de ese
directorio bajo su propio nombre base.

Un origen que es directorio solo se copia con `-r`, que reproduce el
subárbol completo; sin `-r` un operando de directorio se rechaza. Un
archivo de destino existente se sobrescribe por defecto, se omite con
`-n` y se pregunta por la salida de error estándar con `-i` (una
pregunta rechazada omite esa copia sin error; una respuesta ilegible
nunca se considera consentimiento).

El primer fallo detiene la ejecución antes de cualquier operando
posterior. `--` termina el análisis de opciones: cada argumento
posterior es una ruta.

## OPTIONS

- `-r, -R, --recursive` — copiar directorios y su contenido.
- `-f, --force` — cuando un archivo de destino no puede crearse,
  eliminarlo y reintentar la copia una vez.
- `-i, --interactive` — preguntar antes de sobrescribir un archivo
  existente; solo consiente una respuesta que empiece por `y`/`Y`.
- `-n, --no-clobber` — no sobrescribir nunca un archivo existente. El
  último de `-i` y `-n` gana.
- `-l, --link` — dar al destino un segundo nombre del nodo de la fuente
  en lugar de copiar sus bytes, de modo que los dos nombres no puedan
  divergir en una escritura posterior. Una fuente que es directorio sigue
  necesitando `-r`.
- `-s, --symbolic-link` — crear un enlace simbólico que nombre la fuente
  en lugar de copiarla.
- `-P, --no-dereference` — reproducir una fuente que es enlace simbólico
  como un enlace que almacena el mismo destino, literalmente, en lugar de
  copiar lo que nombra (así un enlace relativo o roto sobrevive a la
  copia). Sin ella se sigue un enlace de origen.
- `--preserve=links` — dos fuentes que nombran un mismo nodo obtienen dos
  nombres en el destino en lugar de dos copias, de modo que la copia no
  duplica el almacenamiento en silencio.
- `-d` — `-P` y `--preserve=links` juntas, como en la herramienta GNU.
- `-v, --verbose` — informar de cada copia como `'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — copiar cada origen dentro de
  `dir`, que debe ser un directorio existente. El valor sigue adjunto
  (`-tdir`, `--target-directory=dir`) o como argumento siguiente.
- `-T, --no-target-directory` — tratar el destino como un archivo
  normal; se permite exactamente un origen. No puede combinarse con
  `-t`.
- `-h, -?, --help` — mostrar la ayuda corta de esta orden.

## EXAMPLES

- `cp notes.txt backup.txt` — copiar un archivo con un nombre nuevo.
- `cp -r Projects Archive` — reproducir el árbol `Projects` dentro de
  `Archive` (o como `Archive` si no existe).
- `cp -v -t Backup a.txt b.txt` — copiar ambos archivos a `Backup`,
  informando de cada copia.

## EXIT STATUS

- `0` — todas las copias tuvieron éxito (una omisión por `-n` y una
  pregunta `-i` rechazada no son fallos).
- `1` — un fallo del sistema de archivos, de la pregunta o de la
  salida; el motivo se imprime en la salida de error estándar.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `ls`
- `mv`
- `rm`
