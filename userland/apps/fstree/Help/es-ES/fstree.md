## NAME

fstree — el gestor de archivos en árbol a pantalla completa

## SYNOPSIS

`fstree [directorio]`

## DESCRIPTION

Recorre el sistema de archivos en una sesión a pantalla completa guiada
por teclado: un panel de árbol de directorios a la izquierda y un panel
de archivos a la derecha que lista las entradas del directorio
seleccionado con sus tamaños y fechas de modificación. La sesión comienza
en `directorio` (la vista raíz `/` por omisión).

El árbol se lee de forma perezosa: el contenido de un directorio solo se
obtiene cuando se muestra o se despliega por primera vez, de modo que
recorrer un volumen enorme solo cuesta los directorios realmente
abiertos. Un directorio que el llamante no puede listar se rechaza en el
sitio: el error aparece en la línea de mensajes y la vista anterior se
conserva; nada se fabrica.

Teclas:

- `Arriba`/`Abajo` o `k`/`j` — mover el cursor del panel activo. Mover el
  cursor del árbol lista el directorio recién seleccionado en el panel de
  archivos.
- `Izquierda`/`Derecha` o `h`/`l` — plegar/desplegar la fila del árbol
  bajo el cursor.
- `Intro` — en el árbol, alterna el despliegue; en el panel de archivos,
  desciende al directorio seleccionado (ambos paneles siguen).
- `Tab` — cambiar el panel activo.
- `s` — abrir el menú de ordenación: `n` nombre, `e` extensión,
  `s` tamaño, `m` fecha de modificación, `r` invertir el sentido, `Esc`
  cancela. Los directorios siempre se agrupan antes que los archivos.
- `.` — mostrar/ocultar las entradas ocultas (nombres con punto) en ambos
  paneles.
- `?` — mostrar esta ayuda sobre los paneles; cualquier tecla la cierra.
- `q` — salir restaurando el terminal.

La línea de estado muestra la ruta listada, el número de entradas
visibles, el orden de clasificación, los bytes libres/totales del volumen
subyacente (cuando el servicio de información del sistema puede
informarlos) y si se muestran las entradas ocultas. Un archivo cuyo
formato de almacenamiento no guarda fecha de modificación muestra `-` en
la columna de fecha.

Las operaciones sobre archivos (copiar, mover, renombrar, borrar), el
marcado, la búsqueda y los visores de texto/hexadecimal/desensamblado
llegan en etapas posteriores del plan de la herramienta.

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

ls, du, df
