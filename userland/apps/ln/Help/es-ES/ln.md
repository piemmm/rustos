## NAME

ln — crear enlaces entre archivos

## SYNOPSIS

`ln [-srLPdFfinvT] [-t dir] [--] target... [link_name]`

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

Sin `-s` el enlace es **duro**: una segunda entrada de directorio para
el propio inodo del destino. Ambos nombres alcanzan un solo archivo,
una escritura por cualquiera de ellos se ve por el otro, y el
almacenamiento del archivo subsiste hasta que se retira el último
nombre. Ambos nombres deben estar en un mismo volumen, y un directorio
nunca recibe un segundo nombre — que el árbol de archivos siga siendo
un árbol es lo que da sentido a `..`.

`-r` almacena el destino de un enlace simbólico relativo al directorio
del propio enlace. El sistema de archivos canoniza antes ambas mitades,
así que la diferencia entre ellas es exacta: dos rutas canónicas no
contienen `..` ni enlace alguno. La misma aritmética sobre los operandos
tal como se escribieron nombraría otro objeto en cuanto hubiera un
enlace por medio. `-r` necesita `-s`, porque un enlace duro no almacena
destino que hacer relativo.

`-b`/`-S` se rechazan porque no existe maquinaria de copias de
seguridad.

## OPTIONS

- `-s, --symbolic` — crear enlaces simbólicos en lugar de duros.
- `-r, --relative` — almacenar el destino de cada enlace simbólico
  relativo al directorio del propio enlace. Necesita `-s`.
- `-L, --logical` — enlazar de forma dura lo que nombra un destino
  simbólico, en lugar del propio enlace.
- `-P, --physical` — enlazar de forma dura el destino tal como se
  escribe, sin seguir un enlace simbólico final. Valor por defecto.
- `-d, -F, --directory` — aceptar un operando de directorio. El enlace
  se rechaza igualmente: ningún usuario puede dar un segundo nombre a
  un directorio.
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
