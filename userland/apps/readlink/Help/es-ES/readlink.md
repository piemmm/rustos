## NAME

readlink — mostrar el destino de un enlace simbólico

## SYNOPSIS

`readlink [-nz] [-q | -s | -v] [--] archivo...`

## DESCRIPTION

Muestra el destino que almacena cada operando, uno por operando, en el
orden de la línea de órdenes.

El destino se muestra **tal como está almacenado**. El destino de un
enlace es un dato, no una ruta resuelta al crear el enlace: puede ser
relativo, contener `..` y no nombrar nada. Así que `readlink` muestra la
escritura, y `ls -l` muestra un enlace junto a lo que nombra ahora.

Un operando que **no** es un enlace simbólico no tiene destino que
mostrar —un archivo y un directorio se rechazan ambos con la misma razón
«valor fuera de rango»— y un nombre ausente es «no encontrado». En ambos
casos los operandos restantes se siguen leyendo y la orden termina con un
estado distinto de cero. El silencio es el valor por omisión, como en la
herramienta GNU: `-v` activa los diagnósticos por operando.

`-n` omite el delimitador tras el último destino. Con más de un operando
se ignora, y se informa de ello, porque los delimitadores entre destinos
son lo que los separa.

Se requiere al menos un operando. `--` termina el análisis de opciones.

Las opciones de canonización de GNU `-f`, `-e` y `-m` se **rechazan**, no
se aproximan. Resolver cada componente de una ruta —seguir cada enlace,
tratar `..` físicamente, aplicar el presupuesto de saltos y la regla de
que un enlace no puede salir del volumen que lo almacena— es la única
implementación del sistema de archivos. Una segunda copia aquí podría
mostrar una ruta que el sistema de archivos resuelve de otro modo; por
eso la opción falla hasta que el sistema de archivos ofrezca esa
resolución él mismo.

## OPTIONS

- `-n, --no-newline` — no mostrar el delimitador tras el último destino
  (se ignora, con aviso, para más de un operando).
- `-z, --zero` — terminar cada destino con NUL en lugar de salto de
  línea.
- `-q, -s` — no diagnosticar una lectura rechazada (por omisión;
  también `--quiet`, `--silent`).
- `-v, --verbose` — diagnosticar una lectura rechazada en la salida de
  error estándar.
- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `readlink Home:/Desktop/Notes` — mostrar lo que almacena un atajo.
- `readlink -v alias` — mostrarlo, y decir por qué si no es un enlace.
- `readlink -z a b | tr '\0' '\n'` — destinos separados por NUL para un
  guion.

## EXIT STATUS

- `0` — se mostró el destino de cada operando (o se escribió la ayuda
  breve).
- `1` — al menos una lectura fue rechazada, o la salida falló.
- `2` — no se entendió la línea de órdenes, o nombraba una opción de
  canonización.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
