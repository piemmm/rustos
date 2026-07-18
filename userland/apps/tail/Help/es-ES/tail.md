## NAME

tail — mostrar la última parte de los archivos

## SYNOPSIS

`tail [option...] [file...]`

## DESCRIPTION

Muestra las últimas 10 líneas de cada `file` en la salida estándar. Con
más de un `file`, cada parte va precedida de un encabezado
`==> file <==`. Sin `file`, o cuando `file` es `-`, se lee la entrada
estándar.

`-n` y `-c` cambian cuánto se muestra: un recuento simple (o escrito con
un `-` inicial) muestra las últimas `num` líneas o bytes; un recuento
escrito con un `+` inicial muestra todo **desde** la línea o el byte
`num` (contando desde 1) hasta el final. Un recuento puede llevar un
sufijo multiplicador: `b` (512), `kB` (1000), `K` (1024), `MB`, `M`,
`GB`, `G`, y así para `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (una letra sola
multiplica por potencias de 1024; con `B` por potencias de 1000; con
`iB` por potencias de 1024).

La forma histórica de primer argumento `tail -num` / `tail +num` (con una
letra final `b`/`c`/`l` opcional) se acepta, como en la herramienta GNU.

El modo de seguimiento (`-f`, `-F`, `--follow`, `--retry`, `--pid`,
`--sleep-interval`, `--max-unchanged-stats`) aún no está disponible y se
informa como una opción desconocida: necesita una fuente de despertar
ante cambios de archivo que el sistema todavía no expone, y no se
entrega una espera activa en su lugar.

Cuando no se muestra contenido inicial, se escribe un registro
informativo en el flujo de información estándar (fd 3); nunca cambia la
salida ni el estado de salida. Un archivo que no se puede leer se informa
en la salida de error y la ejecución continúa con el siguiente archivo.

## OPTIONS

- `-c, --bytes <num>` — mostrar los últimos `num` bytes de cada archivo;
  con un `+` inicial, todo desde el byte `num`.
- `-n, --lines <num>` — mostrar las últimas `num` líneas de cada
  archivo; con un `+` inicial, todo desde la línea `num`.
- `-q, --quiet, --silent` — nunca mostrar los encabezados
  `==> file <==`.
- `-v, --verbose` — mostrar siempre los encabezados `==> file <==`.
- `-z, --zero-terminated` — las líneas se delimitan con NUL en lugar de
  salto de línea.
- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `tail log.txt` — mostrar las últimas 10 líneas de `log.txt`.
- `tail -n 3 a b` — mostrar las últimas 3 líneas de `a` y de `b`, cada
  una bajo su encabezado.
- `tail -c 1K image` — mostrar los últimos 1024 bytes de `image`.
- `tail -n +5 notes` — mostrar `notes` desde su 5.ª línea hasta el final.

## EXIT STATUS

- `0` — se mostró cada archivo (o se escribió la ayuda breve).
- `1` — no se pudo leer un archivo, o no se pudo entregar la salida.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `head`
- `cat`
- `wc`
- `man`
