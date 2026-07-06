## NAME

head — mostrar la primera parte de los ficheros

## SYNOPSIS

`head [option...] [file...]`

## DESCRIPTION

Imprime las 10 primeras líneas de cada `file` en la salida estándar.
Con más de un `file`, cada parte va precedida de una cabecera
`==> file <==`. Sin `file`, o cuando `file` es `-`, se lee la entrada
estándar.

`-n` y `-c` cambian cuánto se imprime: un número simple imprime las
primeras `num` líneas o los primeros `num` bytes; un número escrito con
un `-` inicial imprime todo **excepto** las últimas `num` líneas o los
últimos `num` bytes. Un número puede llevar un sufijo multiplicador:
`b` (512), `kB` (1000), `K` (1024), `MB`, `M`, `GB`, `G`, y así
sucesivamente para `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (una letra sola
multiplica por potencias de 1024; con `B` por potencias de 1000; con
`iB` por potencias de 1024).

La forma histórica como primer argumento `head -num` (con los
multiplicadores `b`/`k`/`m` y las letras `l`/`q`/`v`/`z` finales
opcionales) se acepta, como en la herramienta GNU.

Un fichero ilegible se informa en la salida de error y la ejecución
continúa con el fichero siguiente.

## OPTIONS

- `-c, --bytes <num>` — imprimir los primeros `num` bytes de cada
  fichero; con un `-` inicial, todo salvo los últimos `num` bytes.
- `-n, --lines <num>` — imprimir las primeras `num` líneas de cada
  fichero; con un `-` inicial, todo salvo las últimas `num` líneas.
- `-q, --quiet, --silent` — no imprimir nunca las cabeceras
  `==> file <==`.
- `-v, --verbose` — imprimir siempre las cabeceras `==> file <==`.
- `-z, --zero-terminated` — las líneas se delimitan con NUL en lugar
  del salto de línea.
- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `head log.txt` — imprimir las 10 primeras líneas de `log.txt`.
- `head -n 3 a b` — imprimir las 3 primeras líneas de `a` y de `b`,
  cada una bajo su cabecera.
- `head -c 1K image` — imprimir los primeros 1024 bytes de `image`.
- `head -n -1 notes` — imprimir `notes` sin su última línea.

## EXIT STATUS

- `0` — se imprimió cada fichero (o se escribió la ayuda breve).
- `1` — no se pudo leer un fichero, o no se pudo entregar la salida.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `cat`
- `wc`
- `man`
