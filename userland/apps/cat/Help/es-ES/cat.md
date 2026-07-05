## NAME

cat — concatenar archivos en la salida estándar

## SYNOPSIS

`cat [-n] [--] [file...]`

## DESCRIPTION

Lee cada operando de archivo en orden y escribe sus bytes en la salida
estándar. El operando `-` designa la entrada estándar, y sin operando
la entrada estándar es la única fuente.

Con `-n`, las líneas de salida se numeran de forma continua a través
de todas las fuentes, de modo que una línea repartida entre dos
fuentes se numera exactamente una vez, cuando aparece su primer byte.

Una fuente que no se puede leer detiene la orden antes de tocar
cualquier fuente posterior; los bytes ya escritos permanecen escritos.

## OPTIONS

- `-n, --number` — numerar las líneas de salida, de forma continua a
  través de todas las fuentes.
- `-h, -?` — mostrar la ayuda corta de esta orden.

## EXAMPLES

- `cat notes.txt` — escribir `notes.txt` en la salida estándar.
- `cat a.txt - b.txt` — escribir `a.txt`, luego la entrada estándar,
  luego `b.txt`.
- `cat -n log.txt` — numerar cada línea de salida.
- `cat -- -n` — escribir el archivo llamado `-n`.

## EXIT STATUS

- `0` — todas las fuentes fueron escritas.
- `1` — una fuente no pudo leerse, o la salida no pudo entregarse.
- `2` — la línea de órdenes no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `ls`
- `man`
