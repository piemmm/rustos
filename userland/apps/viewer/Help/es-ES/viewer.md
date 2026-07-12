## NAME

viewer — visor gráfico de archivos de solo lectura

## SYNOPSIS

`viewer`

## DESCRIPTION

Abre una ventana de escritorio y pide de inmediato al selector de
archivos de confianza de la sesión de escritorio que elija un archivo.
El visor no posee ninguna capacidad del sistema de archivos: no puede
abrir, listar ni leer nada por sí mismo. La sesión navega en nombre
del visor bajo su propia identidad, y solo el archivo elegido por el
usuario se delega al visor — de un solo uso y de solo lectura.

El contenido del archivo elegido se muestra como texto plano desde la
parte superior de la ventana. Los caracteres imprimibles se muestran
tal cual; cualquier otro byte se representa como un punto. El
contenido mostrado se limita al principio del archivo.

Pulse `Intro` para pedir otro archivo. Cancelar el selector deja el
visor abierto con un aviso. Cerrar la ventana desde el escritorio
termina el visor.

## EXIT STATUS

Cero tras un cierre limpio; distinto de cero cuando el canal de
ventana o la región de fotogramas compartida fue rechazada (el motivo
se indica en el flujo de error estándar).
