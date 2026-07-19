## NAME

fstree — el gestor de archivos en árbol a pantalla completa

## SYNOPSIS

`fstree [directorio]`

## DESCRIPTION

Recorre el sistema de archivos en una sesión a pantalla completa guiada
por teclado, al estilo de XTree Gold: una cabecera de estadísticas de
disco arriba, una ventana de árbol de directorios sobre una ventana de
archivos que lista las entradas del directorio resaltado con sus tamaños
y fechas de modificación. La sesión comienza en `directorio` (la vista
raíz `/` por omisión).

El árbol se lee de forma perezosa: el contenido de un directorio solo se
obtiene cuando se muestra o se despliega por primera vez, de modo que
recorrer un volumen enorme solo cuesta los directorios realmente
abiertos. Un directorio que el llamante no puede listar se rechaza en el
sitio: el error aparece en la línea de mensajes y la vista anterior se
conserva; nada se fabrica.

Teclas:

- `Arriba`/`Abajo` o `k`/`j` — mover el resaltado de la ventana activa
  (`Re Pág`/`Av Pág` una pantalla, `Inicio`/`Fin` al principio o al
  final). Mover el resaltado del árbol lista el directorio recién
  resaltado en la ventana de archivos.
- `Derecha`/`l`/`+` — en el árbol, desplegar la rama resaltada (leída de
  forma perezosa); en la ventana de archivos, descender al directorio
  resaltado.
- `Izquierda`/`h`/`-` — en el árbol, plegar la rama resaltada o — cuando
  ya está plegada o no tiene subdirectorios — subir el resaltado al
  directorio padre; en la ventana de archivos, volver al árbol.
- `Intro` — en el árbol, pasar a la ventana de archivos; en la ventana de
  archivos, abrir la entrada resaltada (un directorio se abre, un archivo
  se muestra).
- `Esc` — cancelar un cálculo de uso de disco en curso; si no, volver de
  la ventana de archivos al árbol.
- `Tab` — cambiar entre la ventana de árbol y la ventana de archivos.
- `s` — abrir el menú de ordenación: `n` nombre, `e` extensión,
  `s` tamaño, `m` fecha de modificación, `r` invertir el sentido, `Esc`
  cancela. Los directorios siempre se agrupan antes que los archivos.
- `c` — copiar la entrada seleccionada: una línea pide el destino. Un
  destino relativo cae en el directorio listado; un destino que es un
  directorio existente recibe la copia dentro, bajo el nombre del
  origen. Un directorio se copia con todo su contenido. Copiar una
  entrada sobre sí misma o un directorio dentro de su propio subárbol
  se rechaza antes de escribir nada.
- `m` — mover la entrada seleccionada, con la misma pregunta de
  destino. Dentro de un mismo volumen el movimiento es un renombrado
  atómico; entre volúmenes se copia la entrada y después se elimina el
  origen.
- `r` — renombrar la entrada seleccionada en el sitio: la línea viene
  precargada con el nombre actual.
- `d` — borrar la entrada seleccionada tras una confirmación; solo `y`
  procede. Borrar un directorio elimina todo su contenido, y la
  confirmación lo dice.
- `M` — crear un directorio en el directorio listado; se pide su nombre.
- `a` — editar los bits de permiso de la entrada seleccionada: una línea
  octal precargada con el modo actual. Intro aplica (solo el propietario
  puede cambiarlo — el núcleo rechaza a cualquier otro), Esc cancela.
- `t` — marcar o desmarcar la entrada seleccionada del panel de
  archivos y bajar una fila; pulsaciones repetidas marcan una serie.
  Las entradas marcadas llevan un `*`.
- `T` — marcar por patrón: un glob (`*`, `?`, `[...]`) comparado con
  los nombres visibles; cada coincidencia se añade al conjunto marcado.
- `i` — invertir las marcas sobre las entradas visibles.
- `C` — borrar todas las marcas.
- `u` — contar el uso de disco bajo el directorio enfocado: archivos,
  bytes y directorios, recorridos de forma incremental en segundo
  plano. `Esc` cancela conservando las cifras contadas hasta entonces.
- `v` — aplanar la rama bajo el directorio enfocado: una lista de cada
  archivo debajo, llenada página a página (`Espacio` carga la
  siguiente). Dentro de la vista, `t`/`T`/`i`/`C` marcan sus filas,
  `c`/`m`/`d` ejecutan operaciones por lotes sobre el conjunto marcado
  y `Esc` vuelve a los paneles. Las filas se nombran relativas a la
  rama aplanada.
- `.` — mostrar/ocultar las entradas ocultas (nombres con punto) en ambos
  paneles.
- `?` — mostrar esta ayuda sobre los paneles; cualquier tecla la cierra.
- `q` — salir restaurando el terminal.

Mientras haya entradas marcadas, `c`, `m` y `d` actúan sobre todo el
conjunto marcado en lugar de la selección: `c`/`m` piden un directorio
de destino existente donde caen las entradas, y `d` confirma el
borrado por lotes. Las entradas se procesan en orden de marcado; un
fallo nunca detiene al resto, el informe final cuenta lo que tuvo
éxito y una pantalla de informe nombra cada fallo — un lote nunca es
parcial en silencio. Las entradas con éxito se desmarcan; los fallos
siguen marcados para reintentar.

Cuando una copia o un movimiento sobrescribiría un archivo existente,
la sesión pregunta por archivo: `o` sobrescribe, `s` lo salta (un
origen saltado queda en su sitio) y `c` cancela los pasos restantes —
en un lote, cancelar abandona todas las entradas restantes —
lo ya aplicado permanece, y el informe final dice qué ocurrió. Un
fallo a mitad de copia elimina el destino a medio escribir y muestra
el error del núcleo; nada se hace pasar jamás por una copia completa.
Cada operación la autoriza el núcleo — un rechazo aparece tal cual en
la línea de mensajes sin que nada cambie.

La línea de estado muestra la ruta listada, el número de entradas
visibles, el orden de clasificación, los bytes libres/totales del volumen
subyacente (cuando el servicio de información del sistema puede
informarlos), si se muestran las entradas ocultas y — mientras haya
algo marcado — el número de entradas marcadas con su total de bytes.
Un archivo cuyo
formato de almacenamiento no guarda fecha de modificación muestra `-` en
la columna de fecha.

La búsqueda y los visores de
texto/hexadecimal/desensamblado llegan en etapas posteriores del plan
de la herramienta.

## OPTIONS

- `directory` — el directorio en el que comienza la sesión; el valor por
  omisión es la vista raíz `/`.
- `-h`, `-?` — imprimir la forma corta de este documento y salir.

## EXIT STATUS

- `0` — la sesión terminó con la `q` del usuario.
- `1` — el directorio inicial no pudo listarse, o falló la vía del
  terminal.
- `2` — los argumentos no pudieron entenderse.

## SEE ALSO

ls, cp, mv, rm, mkdir, chmod, du, df, find
