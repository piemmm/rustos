## NAME

ln — crear enlaces simbólicos

## SYNOPSIS

`ln -s [-finvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Crea un enlace simbólico que nombra cada destino. Con un solo operando
el enlace se crea en el directorio de trabajo con el nombre propio del
destino. Con dos, el segundo operando es un directorio que se rellena
si lo es — o un enlace a uno, salvo con `-n` — y el nombre del enlace en
caso contrario. Con tres o más, el último debe ser ya un directorio.

El destino se guarda **literalmente** y nunca se resuelve: puede ser
relativo, contener `..` y no nombrar nada en absoluto, de modo que un
enlace puede quedar legítimamente colgado. Su gramática sí se comprueba
antes de guardarlo, así que se rechaza un destino que ningún resolutor
podría recorrer. Crear un enlace no otorga ninguna autoridad sobre lo
que nombra: cada uso posterior se autoriza componente a componente bajo
su propia identidad.

Un nombre de enlace ya ocupado se rechaza salvo que `-f` o `-i` indique
reemplazarlo, y el reemplazo **elimina** primero ese nombre, de modo que
nada atraviesa un enlace ya presente hacia lo que señala. Un directorio
nunca se reemplaza.

El primer fallo detiene la ejecución antes de cualquier destino
posterior; los enlaces ya creados permanecen. `--` termina el análisis
de opciones: todo argumento posterior es un operando.

`-s` es obligatorio en este sistema, que no tiene enlaces duros: sin él
`ln` no tiene nada que crear, y lo dice en lugar de crear un enlace
simbólico, que es un objeto distinto. Las opciones exclusivas de los
enlaces duros `-L`, `-P`, `-d` y `-F` se rechazan por la misma razón.
`-b`/`-S` se rechazan porque no existe maquinaria de copias de
seguridad, y `-r` porque calcular un destino relativo al directorio del
enlace exige una resolución canonizadora que este sistema no ofrece —
una léxica nombraría otro objeto en cuanto hubiera un enlace por medio.

## OPTIONS

- `-s, --symbolic` — crear enlaces simbólicos. Obligatorio: véase
  arriba.
- `-f, --force` — eliminar un nombre de enlace existente y crear
  entonces el enlace.
- `-i, --interactive` — preguntar antes de eliminar un nombre de enlace
  existente; solo consiente una respuesta que empiece por `y`/`Y`. Gana
  la última de `-f` e `-i`.
- `-n, --no-dereference` — tratar un destino que es un enlace
  simbólico a un directorio como el simple nombre que también es, en
  lugar de un directorio donde crear los enlaces.
- `-v, --verbose` — informar de cada enlace creado como
  `'link' -> 'target'`.
- `-t dir, --target-directory=dir` — crear cada enlace en `dir`, que ya
  debe ser un directorio. El valor va adjunto (`-tdir`,
  `--target-directory=dir`) o como argumento siguiente.
- `-T, --no-target-directory` — tratar el destino como nombre de
  enlace, nunca como directorio que rellenar; exactamente dos
  operandos. No se puede combinar con `-t`.
- `-h, -?, --help` — mostrar la ayuda breve propia de esta orden.

## EXAMPLES

- `ln -s /System/Commands/ls.app tools/ls` — enlazar un nombre a un
  paquete.
- `ln -s ../shared/notes.txt` — enlazar `notes.txt` aquí a un destino
  relativo.
- `ln -sv -t Links a.txt b.txt` — enlazar ambos archivos en `Links`
  informando de cada enlace.
- `ln -sfn /Storage/media Music` — redirigir un enlace `Music`
  existente a un directorio nuevo, reemplazando el enlace en vez de
  enlazar dentro.

## EXIT STATUS

- `0` — se crearon todos los enlaces (o se escribió la ayuda breve);
  una pregunta `-i` rechazada no es un fallo.
- `1` — cualquier otro caso, con el motivo en la salida de error. Una
  línea de órdenes no entendida también termina con `1`.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `ls`
- `cp`
- `rm`
