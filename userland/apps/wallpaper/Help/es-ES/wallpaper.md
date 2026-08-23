## NAME

wallpaper — selector gráfico de fondo de escritorio

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Abre una ventana de escritorio que ofrece los fondos de pantalla que
vienen con el sistema, el color de fondo detrás de ellos y cómo el
escritorio organiza los iconos en su tablero. Nada cambia en la pantalla
hasta que se aplican los ajustes.

La ventana se controla con el ratón. Una gran vista previa en la parte
superior muestra el fondo de pantalla seleccionado tal como lo dibujará
el escritorio, con el color de fondo elegido en cualquier lugar donde la
imagen no llegue. Debajo, la galería enumera cada fondo de pantalla
incluido como un mosaico: haga clic en uno para seleccionarlo y la vista
previa le seguirá inmediatamente. El mosaico **No wallpaper** (Sin fondo
de pantalla), siempre el primero, muestra solo el color de fondo elegido.

La galería se desplaza cuando contiene más mosaicos de los que muestra la
ventana. Gire la rueda en cualquier lugar de la ventana, arrastre el
control deslizante de la barra de desplazamiento en el borde posterior,
o haga clic en la pista por encima o por debajo del control para
moverse una página cada vez.

Junto a la vista previa hay cuatro ajustes, cada uno de ellos es una
lista desplegable. Haga clic en uno para abrirlo y haga clic en una
opción para seleccionarla:

- **Fit** (Ajuste) — cómo se coloca la imagen: `fill` (cubrir la
  pantalla, recortando el sobrante), `fit` (contenerla completa, color
  de fondo en las barras), `stretch` (distorsionar al tamaño exacto de
  la pantalla), `centre` (tamaño nativo, centrado) y `tile` (repetir
  desde la parte superior izquierda).
- **Backdrop** (Fondo) — el color plano que se muestra donde el fondo
  de pantalla no llega: `Theme` sigue el tema activo del escritorio,
  y los colores con nombre son fijos. Un color que ya esté en efecto y
  que no sea uno de los nombrados se ofrece bajo su propia notación
  `rrggbb`.
- **Icons** (Iconos) — la esquina del tablero desde la cual crece la
  cuadrícula de iconos del escritorio.
- **Sort** (Ordenación) — el orden en que se enumeran los iconos de la
  carpeta del escritorio.

La vista previa es un modelo a escala de su pantalla: tiene la misma
forma que la pantalla y muestra la imagen, el fondo y el ajuste
seleccionados exactamente como los mostrará el escritorio. Lo que ve
en la vista previa es lo que obtendrá.

Las imágenes de fondo de pantalla nunca son decodificadas por este
programa. Cada una es renderizada por un proceso independiente en un
entorno aislado que no tiene autoridad sobre el sistema de archivos, la
red o la ejecución, por lo que una imagen malformada no puede
comprometer al selector ni al escritorio. Un archivo que no se puede
decodificar se marca como `unreadable` en su mosaico y no se vuelve a
intentar.

El teclado alcanza todo lo que hace el ratón. `Tab` y `Shift-Tab` mueven
el foco hacia adelante y hacia atrás a través de la galería, los cuatro
ajustes y los dos botones. Las teclas de flecha se mueven dentro de la
galería, o abren la lista del ajuste enfocado y se mueven dentro de ella.
`Enter` aplica, o activa el botón enfocado, y `Escape` cierra la ventana
sin aplicar los cambios.

Al aplicar, se envían los ajustes elegidos a la sesión de escritorio,
que decide si los adopta, redibuja el tablero y los guarda para el
próximo inicio de sesión. Este programa nunca escribe los ajustes por sí
mismo. El resultado se informa junto a los botones: aplicado, rechazado
con el motivo de la sesión o sin sesión de escritorio escuchando. Un
rechazo deja la ventana abierta con las opciones intactas.

Solo se ofrece el almacén de fondos de pantalla incluidos; no se puede
elegir una imagen de otro lugar del sistema desde esta ventana.

## EXIT STATUS

Cero después de un cierre limpio, incluso cuando los ajustes fueron
rechazados. Distinto de cero cuando la ventana no se pudo abrir, se
rechazó la región de marco compartida o se perdió el canal de la
ventana; el motivo se indica en el flujo de error estándar.

## ENVIRONMENT

Ninguna. Los ajustes con los que se abre la ventana son los ajustes
publicados por la propia sesión de escritorio, leídos a través del
servicio de datos de aplicación en lugar de desde una ruta que nombre
este programa; los escribe la sesión, nunca este programa.

## SEE ALSO

`files`, `viewer`
