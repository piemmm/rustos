## NAME

configure — leer y establecer la configuración del sistema en el arranque

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Lista, muestra y establece los ajustes del almacén de configuración en
`/System/Settings/Configuration/system.conf`. Sin operandos se lista
cada ajuste con su valor actual; con una clave sola se muestra su
valor; con una clave y un valor se cambia el ajuste.

El almacén reside en el volumen raíz cifrado y sus consumidores lo leen
tras desbloquear el sistema de archivos raíz; un cambio surte efecto la
próxima vez que arranque su consumidor (`os.loginType`: el inicio de
sesión del próximo arranque; los conmutadores `cache.*`: el desbloqueo
del próximo arranque).

El conjunto de claves es cerrado: una clave desconocida, o un valor
fuera del conjunto de una clave, se rechaza indicando las opciones
válidas y no cambia nada. Cambiar un ajuste reescribe el almacén en su
forma canónica y requiere acceso de escritura a `/System/Settings`: una
cuenta ordinaria puede leer los ajustes pero no cambiarlos.

- `os.loginType` — `text` o `graphical`: qué tipo de sesión inicia el
  servicio de inicio de sesión para un usuario autenticado. `text` (el
  valor por defecto) inicia el shell de la cuenta — el escritorio puede
  iniciarse bajo demanda con el comando `desktop`; `graphical` inicia
  directamente la sesión de escritorio tras la autenticación cuando hay
  un escritorio instalado, y recurre al texto cuando no lo hay.
- `cache.all` — `on` u `off`: el conmutador maestro de caché. `on` (el
  valor por defecto) deja que cada clase de caché de abajo siga su
  propio ajuste; `off` es un techo que desactiva toda caché en memoria
  sin importar los ajustes por clase.
- `cache.filesystem`, `cache.block`, `cache.transform`,
  `cache.semantic` — `auto` u `off`: los conmutadores por clase para
  las cuatro cachés de memoria recuperables (las cachés del sistema de
  archivos, del bloque de disco completo, del clúster descomprimido y
  del arranque de aplicaciones). `auto` (el valor por defecto) deja que
  el gestor de presión de memoria gobierne la clase; `off` la desactiva
  por completo. No hay un `on` por clase: no se puede forzar a una clase
  a ignorar la presión de memoria. Una clase está efectivamente `off`
  siempre que `cache.all` esté en `off`.

Cada caché es un acelerador recuperable, nunca la fuente de la verdad,
así que apagar cualquiera o todas ellas solo hace más lento el trabajo
afectado — nunca cambia un resultado.

- `net.ipv4.enabled`, `net.ipv6.enabled` — `true` o `false`: los
  conmutadores de familias de direcciones de toda la pila. Ambos son
  `true` de forma predeterminada. Una familia desactivada no vincula
  direcciones, no responde a paquetes y rechaza un socket de esa
  familia con un error tipado — nunca un descarte silencioso.
- `net.ipv6.privacy` — `true` o `false`: si la pila forma direcciones
  IPv6 temporales (de privacidad) además de la estable. `false` (la
  predeterminada) usa solo la dirección SLAAC estable.
- `net.tcp.syncookies` — `auto` o `always`: la defensa contra
  inundaciones SYN. `auto` (la predeterminada) mantiene una cola
  semiabierta acotada y recurre a cookies sin estado al desbordarse;
  `always` responde a cada solicitud de conexión sin estado. No hay
  `off` — una cola de conexiones indefensa no es una opción.
- `net.tcp.keepalive` — `true` o `false`: si las conexiones TCP envían
  sondas de mantenimiento en un enlace inactivo. `false` (la
  predeterminada) nunca sondea ni cierra una conexión inactiva; `true`
  sondea a un par inactivo tras el intervalo habitual y cierra la
  conexión si deja de responder.

La pila de red lee los ajustes `net.*`; un cambio surte efecto cuando
la pila vuelve a aplicar su configuración.

## OPTIONS

- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `configure` — listar todos los ajustes.
- `configure os.loginType` — mostrar el tipo de sesión por defecto.
- `configure os.loginType graphical` — arrancar en el inicio de sesión
  gráfico.
- `configure cache.all off` — desactivar toda caché en memoria en todo
  el sistema.
- `configure cache.filesystem off` — desactivar solo la caché del
  sistema de archivos.

## EXIT STATUS

- `0` — se completó la lista, el valor, la ayuda breve o el cambio.
- `1` — el almacén no se pudo leer o escribir (por ejemplo, quien llama
  no puede cambiar los ajustes del sistema), o la salida no se pudo
  entregar.
- `2` — la línea de órdenes no se entendió, la clave es desconocida o
  el valor está fuera del conjunto de la clave.

## ENVIRONMENT

- `LANG` — el idioma preferido de la ayuda breve (una etiqueta BCP-47
  como `fr-FR`).

## SEE ALSO

- `man`
