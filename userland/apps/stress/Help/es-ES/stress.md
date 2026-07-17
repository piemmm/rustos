## NAME

stress — cargar bajo demanda la CPU, la memoria, el disco y las cachés

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

Lanza procesos de trabajo que cargan la máquina deliberadamente, en el
espíritu de las herramientas establecidas `stress`/`stress-ng`: bucles
de CPU (`--cpu`), trabajadores de memoria asignar-y-tocar (`--vm`),
escritura/sincronización de búferes pequeños (`--io`), escritores de
disco secuenciales grandes (`--hdd`) y relectores que agitan las cachés
(`--cache`, una adición de TAIRiX). Cada trabajador es su propio
proceso paginable; el proceso controlador fija su propia memoria
(`mem_pin`, requiere `CAP_MEM_PIN`) para seguir respondiendo bajo la
presión que él mismo crea, y observa `Ctrl-C`/`Terminate`, de modo que
cada final de la ejecución — finalización, tiempo límite o señal —
detiene a los trabajadores, los recoge y elimina cada archivo de
trabajo.

Los objetivos de memoria y disco se dimensionan según la propia
máquina: salvo cifras explícitas con `--vm-bytes`/`--hdd-bytes`, los
trabajadores vm comparten la mitad de la RAM descubierta y los hdd la
mitad del espacio libre del volumen de trabajo. `--overcommit P`
reescala esos objetivos descubiertos al `P` por ciento del recurso; por
encima de 100 los trabajadores empujan hacia la presión, y los rechazos
tipados que eso produce (volumen lleno, límite de recursos) se cuentan
y se informan como resultados esperados — nunca se reintentan, nunca un
fallo catastrófico. Cargar la máquina no necesita privilegio alguno más
allá de los propios límites de recursos del llamante — los límites son
la defensa, y `stress` los respeta.

Los trabajadores que tocan el disco escriben solo bajo el directorio de
trabajo — el directorio de caché por usuario de la aplicación
(`$HOME/Library/stress`) salvo que `--temp-path` nombre otro — y cada
archivo de trabajo se elimina en el desmontaje, incluidas las rutas de
señal.

Al terminar la ejecución se imprime un resumen (suprimido por
`--quiet`), y se emite un registro `summary` legible por máquina en el
flujo de información estándar consultivo (fd 3).

## OPTIONS

- `--cpu N`, `--io N`, `--vm N`, `--hdd N` — lanzar `N` trabajadores
  del tipo nombrado, con el significado de GNU `stress`.
- `--cache N` — lanzar `N` agitadores de caché (solo TAIRiX: paseos
  fríos repetidos por directorios y relecturas mueven los registros
  de cachés recuperables del núcleo).
- `--all N` — `N` trabajadores de cada tipo.
- `--vm-bytes B`, `--hdd-bytes B` — el objetivo en bytes de cada
  trabajador, con los sufijos GNU (`k`, `m`, `g`, `t`; p. ej.
  `256M`). Los valores por defecto se dimensionan según la RAM / el
  espacio libre descubiertos.
- `--overcommit P` — escalar los objetivos vm/hdd descubiertos al `P`
  por ciento del recurso; puede superar 100 (los rechazos son
  entonces resultados esperados).
- `--timeout T` — detenerse tras `T` (sufijos `s`/`m`/`h`; p. ej.
  `5m`). Sin valor por defecto: sin él, la ejecución continúa hasta
  que una señal la termina.
- `--temp-path DIR` — el directorio de trabajo de los trabajadores
  que tocan el disco.
- `--monitor` — ejecutar `sysmon` en primer plano durante la
  ejecución; esta se informa cuando el monitor termina. Contradice
  `--background`.
- `-q, --quiet` — suprimir el resumen y las líneas de progreso en
  stdout (los errores siguen llegando a stderr).
- `--background` — imprimir el PID del controlador separado y
  devolver el indicador (implica `--quiet`). La forma `&` del shell
  también funciona; esta bandera es para scripts.
- `-h, -?, --help` — mostrar la ayuda corta de este comando y salir.
- `--version` — imprimir el nombre y la versión de la herramienta y
  salir.

## EXIT STATUS

- `0` — la ejecución terminó (los rechazos tipados de los
  trabajadores son resultados esperados y no la hacen fallar).
- `1` — un trabajador falló de verdad, o la ejecución no pudo
  prepararse.
- `2` — la línea de comandos no se entendió.
- `130` / `143` — `Ctrl-C` / `Terminate` terminó la ejecución, tras
  desmontar a los trabajadores y eliminar los archivos de trabajo.

## ENVIRONMENT

- `HOME` — localiza el directorio de trabajo por defecto
  (`$HOME/Library/stress`).
- `LANG` — la configuración regional preferida de la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
