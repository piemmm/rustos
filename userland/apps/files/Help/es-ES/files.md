## NAME

files — navegador gráfico del sistema de archivos

## SYNOPSIS

`files`

## DESCRIPTION

Abre una ventana de escritorio que lista el sistema de archivos,
empezando por la vista raíz. La fila superior muestra la ruta del
directorio actual; las filas inferiores listan las entradas del
directorio, con la entrada seleccionada resaltada con el color de
acento del tema activo. Cada lectura de directorio es un listado
ordinario con comprobación de permisos bajo la identidad del usuario
que lo lanzó: un directorio ilegible se rechaza, nunca se adivina.

El navegador se lanza desde el menú de inicio del escritorio (la
entrada `Files`) o por su nombre desde un shell. Requiere una sesión
gráfica en ejecución: sin ella, el canal de ventana es inalcanzable y
el navegador informa del rechazo por el flujo de error estándar y
termina.

La ventana se maneja con el teclado: `Abajo` y `Arriba` mueven la
selección, `Intro` abre el directorio seleccionado y `Retroceso` sube
al directorio padre. Cerrar la ventana desde el escritorio termina el
navegador.

## EXIT STATUS

Cero tras un cierre limpio; distinto de cero cuando el canal de
ventana, la región de fotogramas compartida o el listado inicial del
directorio fue rechazado (el motivo se indica por el flujo de error
estándar).
