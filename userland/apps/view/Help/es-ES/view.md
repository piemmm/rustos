## NAME

view — visor gráfico de imágenes y documentos

## SYNOPSIS

`view`

## DESCRIPTION

Abre una ventana de escritorio que muestra una imagen o un documento.
Iniciado con un documento — desde el gestor de archivos, o al abrir una
imagen — muestra ese archivo; iniciado por sí solo, pide al selector de
archivos de confianza de la sesión de escritorio que elija uno.

El visor no posee ninguna capacidad sobre el sistema de archivos: no puede
abrir, listar ni leer nada por sí mismo. La sesión navega en su nombre bajo
su propia identidad, y solo el archivo que el usuario elige se le delega, de
un solo uso y solo de lectura. El archivo nunca se decodifica dentro del
visor: sus bytes se transmiten a un proceso de trabajo aparte que no tiene
ningún alcance sobre el sistema de archivos, de modo que un archivo
malformado u hostil no puede alcanzar nada de lo que el visor alcanza.

Los formatos admitidos son JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO y
RISC OS Sprite. Un archivo que el decodificador rechaza declara su motivo en
la ventana y en la salida de error estándar; la ventana nunca se deja en
blanco y nunca se fabrica una imagen.

Varios documentos a la vez son visores separados: comparar dos imágenes una
al lado de la otra es abrir la segunda. Cerrar una ventana termina su visor.

La barra de herramientas superior lleva, en orden: reducir, ampliar, ajustar
a la ventana, tamaño real, entrada anterior, entrada siguiente, girar a la
izquierda, girar a la derecha, reflejar, reproducir o pausar una animación, y
el panel de información. Un deslizador de zoom continuo se sitúa en su borde
final. La línea de estado inferior declara el nombre del documento, su
formato, su tamaño en píxeles, la entrada mostrada, su longitud y la
ampliación.

Arrastre la imagen para moverse por ella cuando sea mayor que la ventana;
mientras lo sea, aparecen barras de desplazamiento en los bordes del lienzo.
Gire la rueda sobre la imagen para desplazarla. Una pulsación secundaria
sobre la imagen abre el menú del visor, que dibuja la sesión de escritorio.

La transparencia se muestra sobre un tablero de ajedrez, para que una imagen
transparente se lea como transparente y no como el color que hay detrás.

* `+` — ampliar al siguiente paso
* `-` — reducir al paso anterior
* `0` — ajustar toda la imagen a la ventana
* `1` — tamaño real, un píxel de imagen por píxel de pantalla
* `2` — ajustar el ancho de la imagen
* `[` / `]` — un cuarto de giro a la izquierda o a la derecha
* `M` — reflejar de izquierda a derecha
* `I` — mostrar u ocultar el panel de información
* `Space` — reproducir o pausar una animación
* `O` — elegir otro documento
* `Page Up` / `Page Down` — entrada anterior o siguiente
* `Home` / `End` — primera o última entrada
* teclas de flecha — moverse por la imagen
* `Escape` — cerrar la ventana

## OPTIONS

`-h`, `-?`, `--help`
: Escribir esta ayuda en la salida estándar y terminar.

## EXIT STATUS

Cero tras un cierre limpio. Distinto de cero cuando se rechazó el canal de
ventana, la región de marco compartida o la sesión de escritorio; el motivo
se declara en la salida de error estándar.
