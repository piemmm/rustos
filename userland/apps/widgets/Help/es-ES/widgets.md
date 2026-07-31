## NAME

widgets — galería de componentes Reactive Alloy

## SYNOPSIS

`widgets`

## DESCRIPTION

Abre una ventana de escritorio que muestra cada control gráfico compartido de
TAIRiX en su propia pestaña: botones, selectores, controles de valor, campos de
texto, controles de elección, colecciones, barras, superficies de
retroalimentación y controles de ventana. Cada pestaña presenta varias
variantes de su familia — distintos roles, estados y valores — para que el
comportamiento completo de cada control sea visible e interactivo en un solo
lugar.

Cambie de pestaña haciendo clic en la barra de pestañas o con las teclas
`Left`, `Right`, `Home` y `End` y `Enter`. Haga clic en un control para
interactuar con él: un conmutador cambia, un deslizador se mueve, un campo de
texto recibe el cursor, un cuadro combinado se abre. Un control pulsado
conserva el foco del teclado, de modo que las flechas, `Enter`, `Space` y los
caracteres escritos lo gobiernan; `Tab` y `Shift+Tab` mueven el foco entre la
barra de pestañas y los controles.

La galería se inicia desde la Biblioteca de programas del escritorio (el
botón `Library` de la barra de tareas) o por su nombre desde un shell.
Requiere una sesión gráfica en curso: sin ella el canal de ventana es
inaccesible y la galería informa del rechazo en el flujo de error estándar y
termina.

## EXIT STATUS

Cero tras un cierre limpio; distinto de cero cuando se rechazó el canal de
ventana o la región de fotogramas compartida (el motivo se indica en el flujo
de error estándar).
