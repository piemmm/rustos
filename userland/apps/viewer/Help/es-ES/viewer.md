## NAME

viewer — visor gráfico de archivos de solo lectura

## SYNOPSIS

`viewer`

## DESCRIPTION

Abre una ventana de escritorio y pide de inmediato al selector de
archivos de confianza de la sesión de escritorio que elija un archivo.
El visor no posee ninguna capacidad del sistema de archivos: no puede
abrir, listar ni leer nada por sí mismo. La sesión navega en nombre
del visor bajo su propria identidad, y solo el archivo elegido por el
usuario se delega al visor — de un solo uso y de solo lectura.

El contenido del archivo elegido se muestra como texto plano desde la
parte superior de la ventana. Los caracteres imprimibles se muestran
tal cual; cualquier otro byte se representa como un punto, de modo que
el contenido binario se vea obviamente saneado. El contenido mostrado
se limita al principio del archivo.

La ventana se maneja con el ratón. Haga clic en el botón **Open…**
(Abrir…) en el encabezado para solicitar otro archivo. Arrastre el
control deslizante de la barra de desplazamiento hacia arriba o hacia
abajo para desplazarse por un archivo largo, haga clic en la pista por
encima o por debajo del control para pasar de página, haga clic en los
botones de los extremos para avanzar una línea o gire la rueda sobre
la ventana para desplazarse. Cancelar el selector deja el visor
abierto con un aviso; cerrar la ventana desde el escritorio termina el
visor.

El teclado es una vía secundaria para las mismas acciones: `Enter`
pide otro archivo, las teclas de flecha avanzan una línea, Page Up/Page
Down avanzan una página y Home/End saltan al principio o al final.

## EXIT STATUS

Cero tras un cierre limpio; distinto de cero cuando el canal de
ventana o la región de fotogramas compartida fue rechazada (el motivo
se indica en el flujo de error estándar).
