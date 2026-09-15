## NAME

sapper — despejar el campo de minas sin pisar ninguna

## SYNOPSIS

`sapper`

## DESCRIPTION

Abre una ventana de escritorio con una cuadrícula de casillas cubiertas. Algunas
ocultan una mina. Cada casilla que descubra y no la oculte indica cuántas de sus
ocho vecinas sí lo hacen, y esos números bastan para deducir dónde están las
minas. Descubra todas las casillas sin mina y habrá ganado; descubra una con mina
y la partida termina.

La primera casilla que descubra siempre es segura, y también las ocho que la
rodean, de modo que una jugada de apertura nunca puede perder y siempre abre una
zona desde la que razonar.

Haga clic en una casilla cubierta para descubrirla. Con el botón secundario
coloca una bandera, otra vez un signo de interrogación si están activados, y otra
vez la borra. Una casilla con bandera está protegida: hacer clic en ella no hace
nada.

Una vez abierta una casilla, su número puede trabajar por usted. Haga clic en un
número cuyas banderas ya coincidan y todas las vecinas restantes se descubren de
una vez. Haga clic con el botón secundario cuando sus vecinas cubiertas sean
exactamente tantas como su número y todas se marcarán de una vez. El botón
central hace lo mismo que el primero, desde cualquier punto de la casilla.

El contador de la izquierda muestra cuántas minas quedan por encontrar, menos las
banderas colocadas; se vuelve negativo si coloca más banderas que minas hay. El
reloj de la derecha arranca con su primera casilla descubierta y se detiene al
terminar la partida. El botón entre ambos inicia una nueva partida, y su cara
dice cómo acabó la anterior.

Las flechas mueven un anillo por el tablero. `Space` descubre la casilla que
contiene, o la acorda si ya está abierta. `F` marca la casilla, `Shift+F` marca
todas sus vecinas cubiertas, `N` inicia una nueva partida, y `1`, `2` y `3`
eligen los tableros principiante, intermedio y experto. Las mismas opciones, y el
ajuste de los signos de interrogación, están en el menú de la barra de iconos del
juego.

Su mejor tiempo en cada uno de los tres tableros estándar se recuerda entre
sesiones. Un tablero de tamaño propio no guarda ninguno, porque dos tableros así
nunca son la misma partida.

El juego se lanza desde la Biblioteca de programas del escritorio, en Games, o
por su nombre desde un intérprete de órdenes. Requiere una sesión gráfica en
marcha: sin ella el canal de ventana es inalcanzable y el juego informa del
rechazo por el flujo de error estándar y termina.

## EXIT STATUS

Cero tras un cierre limpio; distinto de cero cuando se rechazó el canal de
ventana o la región de trama compartida (el motivo se indica en el flujo de error
estándar).
