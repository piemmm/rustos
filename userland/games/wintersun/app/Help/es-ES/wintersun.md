## NAME

wintersun — recorrer un mundo invernal generado proceduralmente

## SYNOPSIS

`wintersun`

## DESCRIPTION

Abre una ventana de escritorio sobre un mundo generado: una vista cenital de
un terreno que la máquina sintetiza en lugar de distribuir, iluminado por un
sol bajo que proyecta sombras largas por cada ladera.

Nada de este mundo se almacena como ilustración. Cada material del que está
hecho el suelo — nieve, roca, grava, brezal, tundra — es un puñado de números
que el cliente convierte en textura al dibujar, de modo que el mundo se ve
igual en todas las máquinas y casi no ocupa espacio en disco. Los caminos se
desgastan sobre lo que cruzan en lugar de posarse encima.

El terreno que aún no se ha generado se dibuja como el hueco que es y se
rellena a medida que llega. El cliente dibuja lo que tiene en lugar de
detenerse a esperar, así que la ventana sigue respondiendo mientras el mundo
se pone al día.

Las flechas o `W`, `A`, `S`, `D` caminan. Dos teclas mantenidas a la vez
recorren la diagonal entre ellas a la misma velocidad, y las teclas opuestas
se anulan. La vista le sigue y se detiene en el borde del mundo en lugar de
salirse de él. Las laderas demasiado empinadas y el agua demasiado profunda le
desvían.

`+` y `-` acercan y alejan la vista, en cinco pasos, desde una celda del mundo
de ocho píxeles de ancho hasta una de ciento veintiocho.

`F11` pone la ventana en pantalla completa y la devuelve después a como
estaba: una ventana maximizada vuelve maximizada. `Esc` la restaura. `Q` sale.

El cliente dibuja con un presupuesto por fotograma. Cuando no puede cumplirlo,
cede detalle en un orden fijo — densidad de partículas, luego la resolución
del búfer de luz, luego el detalle de los materiales, luego las sombras, luego
el tamaño de renderizado — y la tasa de fotogramas nunca es lo que cede. Cada
paso se devuelve cuando los fotogramas llevan un rato holgados. El orden es
fijo para que lo que se ve en una máquina lenta sea previsible y no una
sorpresa.

Una ventana mayor de lo que el renderizador por software puede llenar se
dibuja a 2560×1440 como máximo y se escala hasta el tamaño de la ventana.

## EXIT STATUS

`0` al salir. Un estado distinto de cero indica su motivo en la salida de
error: el mundo no se pudo generar, la ventana no se pudo abrir, o se perdió
el canal de eventos de la sesión.

## SEE ALSO

`sapper`
