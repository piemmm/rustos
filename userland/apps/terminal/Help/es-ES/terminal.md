## NAME

terminal — emulador de terminal gráfico

## SYNOPSIS

`terminal`

## DESCRIPTION

Abre una ventana de escritorio que aloja el shell predeterminado del
usuario en una pantalla de 80×24 caracteres. Las teclas escritas en la
ventana enfocada se envían al shell; todo lo que el shell escribe
(tanto la salida estándar como el error estándar) se interpreta con el
vocabulario ANSI/VT compartido y se dibuja con la paleta del tema
activo. El terminal en sí nunca hace eco: el eco y la edición de línea
pertenecen al shell, exactamente como en una consola.

El terminal se inicia desde el menú de inicio del escritorio (la
entrada `Terminal`) o por su nombre desde un shell. Requiere una
sesión gráfica en ejecución: sin ella, el canal de ventana es
inalcanzable y el terminal informa del rechazo en el flujo de error
estándar y termina.

La sesión termina cuando el shell sale (por ejemplo con `exit`) o
cuando la ventana se cierra desde el escritorio; cerrar la ventana
termina el shell con fin de archivo en su entrada.

## EXIT STATUS

Cero tras un cierre limpio o la salida del propio shell; distinto de
cero cuando el shell no pudo alojarse o cuando el canal de ventana, la
región de fotogramas compartida o el buzón de eventos fue rechazado
(la razón se indica en el flujo de error estándar).
