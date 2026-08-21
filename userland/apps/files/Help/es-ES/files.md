## NAME

files — navegador gráfico del sistema de archivos

## SYNOPSIS

`files [--desktop] [directorio] [-h | -?]`

## DESCRIPTION

Abre una ventana de escritorio que lista el sistema de archivos,
empezando por el `directorio` nombrado en la línea de órdenes, o por el
directorio personal del usuario que la lanzó cuando no se nombra
ninguno. La fila superior muestra la ruta del directorio actual; las
filas inferiores listan las entradas del directorio, con la entrada
seleccionada resaltada con el color de acento del tema activo. Cada
lectura de directorio es un listado ordinario con comprobación de
permisos bajo la identidad del usuario que lo lanzó: un directorio
ilegible se rechaza, nunca se adivina.

El navegador se lanza desde el botón permanente `Files` de la barra de
tareas o por su nombre desde un shell. Requiere una sesión gráfica en
ejecución: sin ella, el canal de ventana es inalcanzable y el navegador
informa del rechazo por el flujo de error estándar y termina.

La ventana se maneja con el teclado: `Abajo` y `Arriba` mueven la
selección, `Intro` abre el directorio seleccionado y `Retroceso` sube
al directorio padre. Cerrar la ventana desde el escritorio termina el
navegador.

El operando `directorio` se trata como entrada no confiable: debe ser
una ruta absoluta dentro del límite de longitud de ruta del sistema, y
cada uno de sus componentes debe ser un nombre de directorio real —
`.` y `..` no lo son, de modo que una escritura nunca puede significar
un lugar distinto del que se lee. Un directorio que incumpla alguna de
esas reglas, o que el usuario que lo lanzó no pueda listar, se rechaza
con el motivo por el flujo de error estándar y la ventana se abre en el
directorio personal, de modo que un argumento incorrecto nunca deja al
usuario sin ventana. Un segundo operando se rechaza de plano en lugar
de ignorarse.

## OPTIONS

- `--desktop` — ejecutarse como el componente de gestor de archivos propio
  del escritorio: una ranura permanente en la barra de iconos que ofrece sus
  lugares y los volúmenes montados, ninguna ventana hasta que se pida una, y
  ninguna forma de salir. La sesión de escritorio pasa esta opción al
  arrancar; nombrar un `directorio` junto a ella se rechaza, porque un componente
  no abre ninguna ventana en la que ponerlo.
- `-h, -?` — mostrar la ayuda corta de esta orden y salir.

## EXIT STATUS

Cero tras un cierre limpio, o tras mostrarse la ayuda corta; `2` cuando
la línea de órdenes no se entendió; por lo demás, distinto de cero
cuando el canal de ventana, la región de fotogramas compartida o el
listado inicial del directorio fue rechazado (el motivo se indica por
el flujo de error estándar).
