## NAME

wallpaper — selector gráfico de fondo de escritorio

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Abre una ventana de escritorio que ofrece los fondos de pantalla que
vienen con el sistema, el color de fondo detrás de ellos y cómo el
escritorio organiza los iconos en su tablero. Nada cambia en la pantalla
hasta que se aplican los ajustes.

La cuadrícula enumera cada fondo de pantalla incluido como una
miniatura, además de una entrada **No wallpaper** (Sin fondo de
pantalla) que muestra solo el color de fondo elegido. Cada miniatura se
representa con el ajuste elegido actualmente, de modo que una vista
previa muestra lo que el escritorio hará realmente con esa imagen. Un
archivo que no se puede decodificar muestra un mosaico de marcador de
posición marcado con su nombre y no se vuelve a intentar.

Las imágenes de fondo de pantalla nunca son decodificadas por este
programa. Cada una es renderizada por un proceso independiente en un
entorno aislado que no tiene autoridad sobre el sistema de archivos, la
red o la ejecución, por lo que una imagen malformada no puede
comprometer al selector ni al escritorio.

Las filas de opciones debajo de la cuadrícula son:

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
- **Icons** (Iconos) — el lado del tablero desde el cual crece la
  cuadrícula de iconos del escritorio.
- **Sort** (Ordenación) — el orden en que se enumeran los iconos de la
  carpeta del escritorio.

La ventana se controla con el teclado. `Tab` y `Shift-Tab` mueven el foco
hacia adelante y hacia atrás a través de la cuadrícula, las filas de
opciones y los botones. Las teclas de flecha se mueven dentro de la
cuadrícula de miniaturas o cambian la opción enfocada. `Enter` activa el
botón enfocado y `Escape` cierra la ventana sin aplicar los cambios.

Al aplicar, se envían los ajustes elegidos a la sesión de escritorio,
que decide si los adopta, redibuja el tablero y los guarda para el
próximo inicio de sesión. Este programa nunca escribe los ajustes por sí
mismo. El resultado se informa en la línea de estado debajo de las filas
de opciones: aplicado, rechazado con el motivo de la sesión o sin sesión
de escritorio escuchando. Un rechazo deja la ventana abierta con las
opciones intactas.

Solo se ofrece el almacén de fondos de pantalla incluidos; no se puede
elegir una imagen de otro lugar del sistema desde esta ventana. Los
clics del puntero no seleccionan nada.

## EXIT STATUS

Cero después de un cierre limpio, incluso cuando los ajustes fueron
rechazados. Distinto de cero cuando la ventana no se pudo abrir, se
rechazó la región de marco compartida o se perdió el canal de la
ventana; el motivo se indica en el flujo de error estándar.

## ENVIRONMENT

`HOME` nombra el directorio de inicio del usuario, bajo el cual se lee
`Settings/Pinboard/pinboard.conf` al inicio para que la ventana se abra
con los ajustes que están en efecto. Ese documento es escrito por la
sesión de escritorio, nunca por este programa. Sin `HOME`, la ventana se
abre con los valores predeterminados.

## SEE ALSO

`files`, `viewer`
