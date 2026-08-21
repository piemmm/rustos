## NAME

link — dar a un archivo un segundo nombre

## SYNOPSIS

`link [--] existente nuevo`

## DESCRIPTION

Crea un enlace duro: `nuevo` pasa a ser un segundo nombre del nodo que
`existente` ya nombra. Ambos nombres alcanzan entonces el mismo archivo
—una escritura por uno se ve por el otro, porque hay un archivo y no una
copia— y el almacenamiento del archivo sobrevive hasta que se elimina el
último de sus nombres.

Deliberadamente no hay opciones. `ln` es la herramienta con `-f`, `-i`,
`-v`, `-s`, `-L`/`-P` y las formas de destino `-t`/`-T`; mantenerlas
separadas significa que un guion que deba crear un solo enlace duro y
nada más dispone de una herramienta que no puede reemplazar un nombre,
seguir un enlace ni crear uno simbólico en su lugar.

Ninguno de los dos nombres se sigue. `existente` es el nodo **tal como se
escribe**, de modo que un enlace simbólico colocado ahí no puede
redirigir el nuevo nombre a su destino (`ln -L` es la herramienta para la
postura que sigue). `nuevo` es un nombre que se crea: uno ocupado se
rechaza, nunca se reemplaza.

Cada rechazo dice algo distinto:

- el nuevo nombre ya existe: una creación nunca reemplaza un nombre;
- `existente` es un **directorio**: un directorio tiene exactamente un
  nombre en todas partes, así que ningún principal puede darle otro;
- los dos nombres están en **volúmenes distintos**: el segundo nombre de
  un nodo debe residir en el volumen que lo almacena;
- el recuento de nombres por nodo del formato se desbordaría;
- el sistema de archivos almacena **un nombre por nodo**: una propiedad
  permanente de ese formato, no un fallo pasajero. Allí use `ln -s` para
  un enlace simbólico.

Se requieren exactamente dos operandos; cualquier otra cosa es un error
de uso y no se crea ningún enlace. `--` termina el análisis de opciones.

## OPTIONS

- `-?, --help` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `link informe.txt informe-copia.txt` — un segundo nombre para un
  archivo.
- `link -- -nombre-raro segundo` — enlazar un nombre que empieza por
  guion.

## EXIT STATUS

- `0` — el enlace se creó (o se escribió la ayuda breve).
- `1` — el sistema de archivos rechazó el enlace, o la salida falló; la
  razón se imprime en la salida de error estándar.
- `2` — no se entendió la línea de órdenes.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

ln, unlink, readlink, ls
