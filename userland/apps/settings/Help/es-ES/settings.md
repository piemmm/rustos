## NAME

settings — configurar el escritorio y esta máquina

## SYNOPSIS

`settings`

## DESCRIPTION

Abre una ventana de escritorio que enumera cada categoría de ajustes de este
sistema: qué es la máquina, el aspecto del escritorio, la pantalla, la red, los
dispositivos con los que se maneja, las cuentas que lo usan y los volúmenes que
contiene. Elegir una categoría en la barra lateral muestra su panel.

Settings no posee autoridad propia. Cada cambio es o una petición a la sesión
de escritorio, dueña de los ajustes del usuario, o una ejecución
reautenticada del comando que ya escribe ese almacén; nada de aquí puede
elevar un privilegio.

Una categoría que este sistema no puede atender lo dice con claridad y nombra
lo que tendría que existir para poder hacerlo. Nunca se muestra un control que
no cambiaría nada.

Escriba en el campo de búsqueda sobre la barra lateral para filtrarla a las
categorías y ajustes que alcanza una palabra. `Tab` y `Shift+Tab` mueven el
foco entre el campo de búsqueda, la ruta, la barra lateral y el panel; `Up` y
`Down` recorren la barra lateral y `Enter` abre la fila. Una ventana demasiado
estrecha descarta la barra lateral, y la primera miga de la ruta enumera
entonces las categorías.

Se lanza desde la fila *Settings…* del menú de sistema del escritorio, desde la
Biblioteca de programas, o por su nombre desde un intérprete de órdenes.
Requiere una sesión gráfica en marcha: sin ella el canal de ventana es
inalcanzable y comunica el rechazo en el flujo de error estándar y termina.

## EXIT STATUS

Cero tras un cierre limpio; distinto de cero cuando se rechazó el canal de
ventana o la región de fotogramas compartida (el motivo se indica en el flujo
de error estándar).
