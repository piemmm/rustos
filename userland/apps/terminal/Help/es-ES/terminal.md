## NAME

terminal — emulador de terminal gráfico

## SYNOPSIS

`terminal`

## DESCRIPTION

Abre una ventana de escritorio que aloja el shell predeterminado del
usuario en una pantalla de 80×25 caracteres. Las teclas escritas en la
ventana enfocada se envían al shell; todo lo que el shell escribe
(tanto la salida estándar como el error estándar) se interpreta con el
vocabulario ANSI/VT compartido y se dibuja con el esquema de colores
elegido en la configuración. El terminal en sí nunca hace eco: el eco y
la edición de línea pertenecen al shell, exactamente como en una
consola.

La ventana se abre con las medidas que la pantalla de 80×25 tiene en el
tamaño de texto vigente, para que se ajuste a la pantalla en la que se
muestra; en una pantalla demasiado pequeña para ese tamaño, el texto se
reduce en lugar de estrechar la ventana, porque un programa que se
diseña para 80 columnas debe seguir obteniéndolas.

El terminal se inicia desde la Biblioteca de programas del escritorio
(el botón `Library` de la barra de tareas) o por su nombre desde un
shell. Requiere una sesión gráfica en ejecución: sin ella, el canal de
ventana es inalcanzable y el terminal informa del rechazo en el flujo de
error estándar y termina.

La sesión termina cuando el shell sale (por ejemplo con `exit`) o
cuando la ventana se cierra desde el escritorio; cerrar la ventana
termina el shell con fin de archivo en su entrada.

Al pulsar el botón secundario (derecho) del ratón en cualquier lugar de
la pantalla se abre el menú del terminal. Cada fila tiene un atajo de
teclado que funciona esté o no abierto el menú, y `Escape` —o un clic
fuera del menú— lo descarta sin elegir nada.

| Fila | Atajo | Qué hace |
| --- | --- | --- |
| Configuración… | `Ctrl ,` | Abre la configuración descrita abajo. |
| Texto más grande | `Ctrl +` | Dibuja la pantalla un paso más grande. |
| Texto más pequeño | `Ctrl -` | Dibuja la pantalla un paso más pequeño. |
| Tamaño real | `Ctrl 0` | Vuelve al tamaño de texto predeterminado. |
| Limpiar pantalla | `Ctrl Shift K` | Vacía la pantalla sin escribir en el shell. |
| Cerrar | `Ctrl Shift W` | Cierra la ventana y termina el shell. |

La configuración se abre en la propia ventana y tiene dos pestañas.
**Apariencia** elige el esquema de colores, establece el tamaño del
texto y edita el esquema propio del usuario. Los esquemas incluidos son
*System* (que sigue la apariencia oscura o clara del escritorio),
*Midnight*, *Phosphor*, *Amber*, *Ember*, *Contrast*, *Paper* y
*Custom*. Elegir *Custom* utiliza los colores editados debajo del
selector: una cuadrícula de los veinte colores de los que se dibuja una
pantalla —el fondo, el primer plano, el cursor, el texto del cursor y
los dieciséis colores ANSI— con controles deslizantes de rojo, verde y
azul para el que esté seleccionado.

**Efectos** establece cómo se dibuja la pantalla.

| Efecto | Qué hace |
| --- | --- |
| Opacidad | Cómo de sólido es el fondo. Por debajo del total, el escritorio se ve detrás del texto, que sigue siendo totalmente legible. |
| Desenfoque de fondo | Cuánto se desenfoca el escritorio detrás de una ventana transparente. No tiene efecto en una ventana totalmente opaca. |
| Líneas de escaneo | Atenúa las filas alternas, la parte plana del aspecto de una máscara de sombra. |
| Resplandor | Difunde la luz de los píxeles brillantes a su alrededor, de modo que el texto lleva el halo suave de un tubo muy excitado. |
| Ruido | Un suelo de ruido por píxel en movimiento, como el que tiene una señal analógica. |
| Fósforo | Cuánto tiempo persisten los píxeles encendidos, de modo que el texto que se desplaza rápido deja un rastro. |
| Bamboleo | Un ligero vaivén horizontal errante, como el de un tubo fuera de tiempo. |

Cada cambio surte efecto inmediatamente y se guarda en el perfil propio
del usuario, de modo que un terminal posterior se abra de la misma forma.
El sistema operativo custodia el perfil a través de su servicio de
configuración, y es privado del terminal: ninguna otra aplicación puede
leerlo ni modificarlo. Solo se almacena lo que el usuario cambió
realmente, así que *Restaurar valores predeterminados* elimina esas
elecciones en lugar de congelar los valores de hoy: entonces se aplica lo
que cambie el administrador o una versión posterior del terminal. Un ajuste
que el terminal no puede interpretar se queda en su valor predeterminado y
se indica en el flujo de error estándar, y un servicio de configuración
inalcanzable deja al terminal funcionando con los valores con los que se
distribuye, lo que también se indica.

## EXIT STATUS

Cero tras un cierre limpio o la salida del propio shell; distinto de
cero cuando el shell no pudo alojarse o cuando el canal de ventana, la
región de fotogramas compartida o el buzón de eventos fue rechazado
(la razón se indica en el flujo de error estándar).

## ENVIRONMENT

`HOME`
: El directorio personal de la cuenta, donde el terminal lee y escribe
su perfil. Sin él, el terminal funciona con el perfil predeterminado y
no guarda nada.

`TERM`
: Se exporta al shell alojado con el valor `xterm-256color`, que nombra
el emulador que este terminal presenta. Cualquier valor heredado se
sustituye; el resto del entorno se transmite al shell sin cambios.

## SEE ALSO

`elsh`, `sysinfo`
