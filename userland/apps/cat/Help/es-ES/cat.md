## NAME

cat — concatenar archivos en la salida estándar

## SYNOPSIS

`cat [-AbeEnstTuv] [--] [file...]`

## DESCRIPTION

Lee cada operando de archivo en orden y escribe sus bytes en la
salida estándar. El operando `-` designa la entrada estándar, y sin
operando la entrada estándar es la única fuente.

Con `-n`, las líneas de salida se numeran de forma continua a través
de todas las fuentes, de modo que una línea que abarca dos fuentes se
numera exactamente una vez, cuando aparece su primer byte. `-b`
numera solo las líneas no vacías y prevalece sobre `-n`. `-s`
suprime las líneas en blanco adyacentes repetidas; una línea
suprimida no se escribe ni se numera.

Las opciones de marcado hacen visibles los bytes invisibles: `-E`
imprime `$` antes de cada salto de línea, `-T` imprime TAB como `^I`,
y `-v` imprime los demás bytes de control como `^X` y los bytes no
ASCII en notación `M-`. `-e`, `-t` y `-A` son las combinaciones
habituales `-vE`, `-vT` y `-vET`.

Una fuente que no puede leerse detiene el comando antes de tocar
cualquier fuente posterior; los bytes ya escritos permanecen
escritos.

## OPTIONS

- `-A, --show-all` — equivalente a `-vET`.
- `-b, --number-nonblank` — numerar las líneas de salida no vacías;
  prevalece sobre `-n`.
- `-e` — equivalente a `-vE`.
- `-E, --show-ends` — imprimir `$` al final de cada línea.
- `-n, --number` — numerar las líneas de salida, de forma continua a
  través de todas las fuentes.
- `-s, --squeeze-blank` — suprimir las líneas en blanco adyacentes
  repetidas.
- `-t` — equivalente a `-vT`.
- `-T, --show-tabs` — imprimir los caracteres TAB como `^I`.
- `-u` — aceptado e ignorado; la salida ya no usa búfer.
- `-v, --show-nonprinting` — usar la notación `^` y `M-` para los
  bytes de control y no ASCII, excepto el salto de línea y TAB.
- `-h, -?` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `cat notes.txt` — escribir `notes.txt` en la salida estándar.
- `cat a.txt - b.txt` — escribir `a.txt`, luego la entrada estándar,
  luego `b.txt`.
- `cat -n log.txt` — numerar cada línea de salida.
- `cat -bs draft.txt` — numerar las líneas no vacías y compactar las
  series de líneas en blanco.
- `cat -A config.txt` — hacer visibles los finales de línea, las
  tabulaciones y los bytes de control.
- `cat -- -n` — escribir el archivo llamado `-n`.

## EXIT STATUS

- `0` — todas las fuentes fueron escritas.
- `1` — una fuente no pudo leerse, o la salida no pudo entregarse.
- `2` — la línea de comandos no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta
  (una etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `ls`
- `man`
