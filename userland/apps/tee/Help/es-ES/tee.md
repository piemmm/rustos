## NAME

tee — leer de la entrada estándar y escribir en la salida estándar y en ficheros

## SYNOPSIS

`tee [opción...] [fichero...]`

## DESCRIPTION

Copia la entrada estándar a la salida estándar y a cada fichero
nombrado, de modo que los datos de una tubería puedan verse y
capturarse a la vez. Cada fichero se crea si no existe y se
sobrescribe, salvo que `-a` añada al final. Un fichero que no puede
abrirse o escribirse se notifica y la ejecución continúa con las
salidas restantes, según el modo `--output-error` elegido.

TAIRiX no tiene `SIGPIPE`: la desaparición de un consumidor se
manifiesta como un error de escritura en la salida estándar — la única
salida de esta orden que puede ser una tubería —, así que la «tubería»
de los modos GNU significa aquí exactamente esa salida. Sin
`--output-error`, una salida estándar fallida detiene la ejecución (el
equivalente de la herramienta GNU muriendo por `SIGPIPE`, con la razón
indicada en el error estándar); con un modo `-nopipe` se tolera en
silencio.

GNU `tee -i` (ignorar interrupciones) no está disponible: TAIRiX no
tiene disposición de señales por proceso que configurar. El conmutador
llegará con ese trabajo del núcleo en lugar de aceptarse e ignorarse.

## OPTIONS

- `-a, --append` — añadir al final de los ficheros nombrados; no
  sobrescribirlos.
- `-p` — tolerar en silencio una salida estándar fallida; lo mismo que
  `--output-error=warn-nopipe`.
- `--output-error[=<mode>]` — cómo se trata una salida fallida. Sin
  valor, `warn-nopipe`. Los modos (se acepta un prefijo no ambiguo):
  `warn` — notificar un error de escritura en cualquier salida,
  descartar esa salida y continuar; `warn-nopipe` — como `warn`, pero
  una salida estándar fallida se descarta en silencio y no cambia el
  estado de salida; `exit` — notificar un error de escritura en
  cualquier salida y detenerse; `exit-nopipe` — como `exit`, pero una
  salida estándar fallida se descarta en silencio.
- `-h, -?` — mostrar la ayuda corta de esta orden.
- `--` — terminar el análisis de opciones; cada argumento posterior
  nombra un fichero, y un operando `-` nombra un fichero llamado `-`.

## EXAMPLES

- `ls -l | tee listing.txt` — mostrar el listado y guardar una copia.
- `make 2>&1 | tee -a build.log` — añadir la transcripción de una
  compilación mientras se observa.
- `cat data | tee copy1 copy2 | wc -c` — capturar dos copias y contar
  los bytes que siguen fluyendo.

## EXIT STATUS

- `0` — todas las salidas se sirvieron hasta el final de la entrada (o
  se mostró la ayuda corta solicitada); un fallo de la salida estándar
  tolerado por un modo `-nopipe` no lo cambia.
- `1` — una salida falló de una manera que el modo elegido cuenta, o la
  entrada no pudo leerse.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `cat`
- `head`
- `wc`
