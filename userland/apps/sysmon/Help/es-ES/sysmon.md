## NAME

sysmon — observar en vivo la memoria, las cachés y la carga del núcleo

## SYNOPSIS

`sysmon [-d seg.décimas] [-h | -?]`

## DESCRIPTION

`sysmon` es una vista en vivo a pantalla completa de lo que el núcleo
hace con la memoria y la CPU, leída por completo a través de la API de
información del sistema — no hay `/proc` que raspar. Muestra la memoria
física y su composición, el montículo del núcleo, la banda de presión de
memoria con su historial reciente, el registro de cachés recuperables con
las **proporciones de aciertos** por clase, el nivel comprimido `ramzip`,
el total de memoria fijada, el uso de almacenamiento de los volúmenes
montados, la carga por CPU, la tabla de interrupciones del núcleo y un
censo de procesos. Sigue siendo utilizable mientras el sistema está bajo
carga deliberada y reposa entre refrescos cuando está ocioso (la lectura
se aparca; nunca gira en vacío).

Al arrancar, el monitor fija su propia memoria (`mem_pin`, que requiere
`CAP_MEM_PIN`) para no detenerse nunca en sus propios fallos de página
bajo la misma presión que observa. Una fijación rechazada se informa en
la línea de título y la sesión continúa sin fijar — la fijación es
accesoria, nunca fatal.

La pantalla se refresca en cada intervalo (3,0 segundos salvo que `-d` lo
cambie). El monitor no acepta operandos: se controla con teclas dentro de
la sesión.

- `q` — salir.
- Izquierda / Derecha (o `p`) — cambiar el panel de detalle (Izquierda =
  anterior, Derecha / `p` = siguiente): cachés, el nivel comprimido, el
  almacenamiento de los volúmenes montados (discos), la carga por CPU,
  las líneas de interrupción, los procesos.
- `r` — refrescar ahora.
- `+` / `-` — alargar / acortar el intervalo en un segundo, entre 0,1 y
  60 segundos.
- Arriba/Abajo, RePág/AvPág, Inicio/Fin — desplazar el panel enfocado.
- `h`, `?` — mostrar u ocultar el resumen de teclas de la sesión (que
  reproduce la leyenda de las barras de abajo).

### El bloque de resumen

Un bloque de resumen fijo precede al panel de detalle. Cada línea lleva
una etiqueta a la izquierda para que se lea sin color; el color solo
refuerza.

- **Línea de título** — el nombre de la herramienta, el tiempo de
  actividad del sistema (`up D days, H:MM`), las tres medias de carga
  (1/5/15 minutos) y el estado de fijación (`[pinned]`, o
  `[unpinned: <reason>]` cuando la fijación fue rechazada).
- **`Mem`** — la barra de memoria (véase la leyenda de barras), seguida
  de MiB usados / totales, el porcentaje usado, el tamaño del montículo
  del núcleo y — cuando no son cero — las cifras del almacén comprimido
  `ramzip` y de la memoria fijada `pinned`.
- **`Pres`** — la barra de presión de memoria: un indicador de cinco
  bandas, cada banda alcanzada rellena con su propio color de severidad,
  seguida del nombre de la banda actual, las cifras de libre / reserva y
  el total de entradas en banda.
- **`Hist`** — la tira del historial de bandas de presión: un glifo por
  refresco, el más antiguo a la izquierda, cada uno coloreado por su
  banda — `.` normal, `-` leve, `=` moderada, `#` severa, `!` crítica —
  de modo que un tramo de presión se lee como una racha coloreada.
- **`CPU`** — la barra global de CPU (véase la leyenda de barras),
  seguida del porcentaje ocupado de todas las CPU, el número de CPU y los
  contadores sumados de cambios de contexto y de expropiaciones.
- **`Tasks`** — el censo de procesos: totales, en ejecución, durmiendo,
  detenidos y zombis (con `(own)` añadido cuando se rechazó el censo de
  todos los procesos y solo se cuentan las tareas propias).
- **Barra de pestañas de paneles** — cada panel de detalle, con el
  enfocado resaltado, y un indicador de desplazamiento a la derecha
  cuando el panel enfocado desborda.

### La leyenda de barras

Los indicadores `Mem` y `CPU` son barras entre corchetes `[…]`. El
resumen de `?` reproduce esta leyenda dentro de la sesión en marcha.

La barra de memoria (`Mem`) es una barra **apilada** cuyas celdas nombran
lo que contiene la memoria física — un reparto *disjunto* de la memoria
usada (`used` es `total` menos `free`), de modo que nada se cuenta dos
veces y el ancho relleno es exactamente la fracción usada:

- `#` — memoria residente de usuario (verde): páginas residentes en los
  espacios de direcciones de usuario.
- `K` — el montículo del núcleo (cian): los montículos y las losas
  propios del núcleo.
- `=` — otra memoria en uso (magenta): todo lo usado pero no atribuido
  arriba (cachés de páginas, búferes, marcos del núcleo).
- en blanco — memoria libre.

El almacén comprimido `ramzip` y la memoria anónima `pinned` se solapan
con esos cubos (las páginas fijadas son residentes de usuario; el almacén
comprimido es memoria del núcleo), así que se informan como cifras al
lado de la barra en lugar de como segmentos separados que contarían dos
veces — contabilidad honesta antes que una imagen engañosa.

La barra de presión (`Pres`) colorea cada banda según su profundidad:
normal/leve verde, moderada amarilla, severa/crítica roja.

La barra de CPU (`CPU`) se rellena con celdas ocupadas `#` sobre pista
ociosa en blanco, coloreada según la cuota ocupada (verde por debajo del
60 %, amarillo por debajo del 85 %, rojo al 85 % o más). TAIRiX contabiliza
el tiempo de CPU solo como ocupado frente a ocioso — no hay reparto
usuario/sistema/e/s en la API — así que la barra muestra una única
categoría honesta de ocupación, con el detalle por núcleo en el panel
`cpu`.

### Los paneles de detalle

Izquierda / Derecha (o `p`) recorre seis paneles. Cada uno tiene una
cabecera de columna invertida (vídeo inverso, negrita) para que el
encabezado se lea como una barra distinta sobre el cuerpo.

### caches — el registro de cachés recuperables

Son las cachés que el núcleo puede devolver para aliviar la presión de
memoria **sin pérdida de datos**: cada entrada se puede reconstruir desde
su fuente canónica, así que el núcleo la descarta en lugar de paginarla.
El panel es la respuesta directa a «¿están las cachés haciendo su
trabajo?»: cada fila es una clase de recuperación, agregada sobre todas
las cachés registradas, y lleva su propia **proporción de aciertos**.

Columnas:

- `class` — la clase de recuperación (véase la lista de clases abajo).
- `entries` — entradas vivas retenidas actualmente para la clase.
- `cached` — la huella residente de la clase: la carga útil de las
  entradas más los metadatos de contabilidad por entrada, juntos.
- `hits` — búsquedas de la clase servidas desde la caché desde el
  arranque (la caché evitó la fuente canónica).
- `misses` — búsquedas de la clase que cayeron a la fuente canónica desde
  el arranque.
- `hit%` — la proporción de eficacia de la caché, `hits / (hits +
  misses)` como porcentaje entero. Una proporción alta significa que la
  caché rentabiliza su memoria; una baja, que retiene memoria sin evitar
  trabajo. Muestra `-`, nunca un `0%` fabricado, para una clase que nada
  ha buscado en este arranque (un denominador ocioso).
- `ref` — admisiones **rechazadas** desde el arranque (una entrada que la
  caché declinó retener: fuera de presupuesto, no contabilizable, o sin
  memoria).
- `shr` — pasadas de **encogimiento** forzado por presión que recuperaron
  entradas de la clase desde el arranque.
- `fail` — **fallos** internos atribuidos a la clase: un defecto de
  registro detectado que envenenó (desactivó fail-closed) una caché.

Los recuentos se abrevian por encima de 99 999 como `k`/`M`/`G`/`T`
(millares decimales, no KiB) para que una columna nunca se ensanche.

Las clases de recuperación, en el orden en que el núcleo las recupera
bajo presión (la primera de la lista se descarta primero, así que una
caché baja en la lista sobrevive más):

- `disposable-ui` — estado de interfaz desechable (recursos
  rasterizados, atlas de glifos, instantáneas de ventana): lo más barato
  de perder, lo primero en irse.
- `predictive-prefetch` — datos precargados de forma especulativa
  (listados, miniaturas, índices de compleción): nunca necesarios para la
  corrección.
- `background-validation` — productos de trabajo de validación en tiempo
  ocioso (progreso de escaneo, huellas candidatas): el trabajo
  especulativo se detiene al empezar la presión.
- `semantic-app-cache` — estado verificado de lanzamiento de aplicaciones
  (manifiestos analizados, resúmenes de validación, resultados de
  resolución de órdenes). Recuperarlo nunca puede impedir lanzar una
  aplicación — la puerta de carga simplemente se vuelve a ejecutar.
- `runtime-cache` — estado derivado propiedad del runtime (preparación
  del cargador, mapas de recursos): agrupado con la caché semántica.
- `clean-file-data` — *contenido* de archivo limpio y reconstruible,
  releíble desde el volumen: una lectura de dispositivo acotada
  reconstruye un trozo. Recuperado antes de comprimir nada en `ramzip`.
- `transform-cache` — formas intermedias costosas de datos autorizados
  (datos de clúster verificados, descifrados, descomprimidos): más
  costosas de reconstruir que una lectura limpia, así que se recuperan
  tras los datos de archivo limpios.
- `fs-metadata` — metadatos del sistema de archivos: registros de estado,
  resultados de búsqueda de nombres, entradas de directorio y registros
  de seguridad. Pequeños, calientes y reconstruidos solo por un recorrido
  del árbol en varios pasos, así que sobreviven a los datos de archivo
  bajo presión.
- `reliability-assist` — estado reconstruible de asistencia a la
  recuperación (ventanas de verificación, resúmenes de salud):
  justificado por la latencia de recuperación, así que se preserva el más
  tiempo.

### ramzip — el nivel de memoria comprimida

`ramzip` comprime páginas anónimas frías en un almacén menor en RAM en
lugar de paginarlas. Sus secciones:

- `tier` — la huella viva: `entries` retenidas, bytes `logical` (sin
  comprimir) representados, bytes `stored` (texto cifrado) realmente
  retenidos y bytes `metadata` de contabilidad; luego `saved` (lógico
  menos almacenado) con su porcentaje de lo lógico — la memoria que el
  nivel recupera.
- `capacity` — los topes derivados a los que el nivel se dimensiona:
  `min` (siempre disponible), `soft` (objetivo), `hard` (techo) y los
  bytes `pinned` actuales.
- `compress` — la ruta de almacenamiento (escritura): `attempts`
  ofrecidos, `accepted` y almacenados, y la **tasa de aceptación**
  (aceptados / intentos) — la proporción de aciertos de este nivel para
  la compresión. Debajo, el desglose de rechazos: incompresible, política,
  tope, no elegible, reserva, cuota de tarea y refusos por thrash.
- `restore` — la ruta de recuperación (lectura): `faults` de página,
  restauraciones `warm`, restauraciones `clustered` y su total
  `restored`; luego los `failures` (autenticación / decodificación) y la
  **tasa de éxito** (restaurados / (restaurados + fallos)). Cada
  proporción es un porcentaje, o `-` para un denominador ocioso.
- `warm-up` — los `attempts` del restaurador cálido en segundo plano, su
  recuento `stopped` y su recuento `thrash-detected`.

### disks — almacenamiento de los volúmenes montados

Una fila estilo `df` por volumen montado: punto de montaje, tipo de
sistema de archivos, tamaño total, usado, disponible, porcentaje de uso y
una barra de uso ASCII. Un volumen cuyo controlador no informa capacidad
muestra `capacity unknown` en lugar de un tamaño fabricado; un volumen
retirado por sorpresa o en conflicto de recuperación se dibuja en la
representación de aviso y se marca (`[unavailable-dirty]`,
`[unavailable-lost]`, `[recovery-conflict]`). No hay contadores de
rendimiento de e/s por dispositivo en la API, así que esto es capacidad y
uso honestos, no tasas de transferencia fabricadas.

### cpu — carga por CPU

Una fila por CPU: su cuota ocupada en el intervalo (`busy%`), la
profundidad de su cola de ejecución (`queue`) y sus cuentas de
cambios de contexto (`switches`) y de expropiaciones (`preemptions`) desde
el arranque.

### irqs — líneas de interrupción

Una fila por línea de interrupción vinculada, en orden ascendente de
línea: el id de la línea, la tarea controladora propietaria (`owner`), el
`count` de interrupciones desde el arranque y el `state` de la línea —
`active`, o `quarantined` (dibujado en la representación de aviso) cuando
la red de seguridad del núcleo contra líneas desbocadas la ha desactivado.

### procs — el censo de procesos

Los mayores consumidores por `%cpu` y por memoria (`size`), cada uno con
su pid, su orden y — en la tabla de memoria — su estado. La lista
interactiva completa de procesos es tarea de `top`; esto es solo el
resumen del censo.

### Capacidades

Cada cifra viaja por la API de información del sistema. Las consultas de
estadísticas del núcleo (memoria, presión, cachés, `ramzip`, carga por
CPU) requieren `CAP_SYSINFO_KERNEL`; el panel de líneas de interrupción
requiere `CAP_SYSINFO_HW`; el censo de todos los procesos requiere
`CAP_SYSINFO_GLOBAL`. Quien carezca de una ve el rechazo de ese panel
explicado — nunca una cifra fabricada — mientras el resto de la sesión
continúa (fallar cerrado, degradar con gracia). El almacenamiento de los
volúmenes montados no está restringido.

## OPTIONS

- `-d, --delay <seconds>` — el intervalo entre refrescos automáticos, en
  segundos con fracción opcional (solo se conserva el primer dígito
  decimal, las décimas): `sysmon -d 1.5` refresca cada 1,5 segundos. Por
  defecto 3,0. GNU `top` acepta un intervalo cero y refresca tan rápido
  como puede; TAIRiX nunca gira en vacío, así que un cero se eleva al
  mínimo de 0,1 s.
- `-h, -?` — mostrar la ayuda breve de esta orden y salir. Dentro de una
  sesión en marcha, las mismas teclas alternan el resumen de teclas.

## EXIT STATUS

- `0` — la sesión terminó con `q`, o se mostró la ayuda breve.
- `1` — el terminal falló; la razón se escribe en la salida de error.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda breve (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
