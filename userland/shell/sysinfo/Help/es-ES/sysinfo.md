## NAME

sysinfo — consultar información del sistema

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Emite una consulta tipada a la API de información del sistema y muestra
la respuesta. TAIRiX no tiene `/proc` ni `/sys`: este comando es la
cara de terminal de la misma API versionada y controlada por
capacidades que usa todo programa, y ningún camino elude el control de
capacidad.

Las consultas:

- `processes`, `ps` — listar procesos, una fila por proceso.
- `memory`, `mem` — estadísticas de memoria del núcleo (necesita
  `CAP_SYSINFO_KERNEL`).
- `hardware`, `hw` — el árbol de hardware detectado (necesita
  `CAP_SYSINFO_HW`).
- `identity`, `id` — identidad de la máquina y versión del SO.
- `uptime` — tiempo desde el arranque y la hora de arranque.
- `limits`, `rlimits` — sus límites de recursos efectivos y su uso en
  vivo.
- `seats` — el inventario de asientos: el propietario de cada pantalla
  y su consola en primer plano (necesita `CAP_SYSINFO_HW`).
- `pressure` — el indicador de presión de memoria en vivo: banda,
  umbrales y contadores de transición (necesita `CAP_SYSINFO_KERNEL`).
- `reclaim` — el libro de cachés recuperables, una fila por clase
  (necesita `CAP_SYSINFO_KERNEL`).
- `ramzip` — los contadores del nivel de memoria comprimida (necesita
  `CAP_SYSINFO_KERNEL`).
- `cpu` — profundidad de cola, cambios de contexto y expropiaciones por
  CPU (necesita `CAP_SYSINFO_KERNEL`).
- `irq`, `irqs` — la tabla de IRQ del núcleo: una fila por cada línea de
  interrupción vinculada — su identificador, la tarea del controlador
  propietaria, el número de interrupciones desde el arranque y si la
  línea está en cuarentena (necesita `CAP_SYSINFO_HW`).
- `help` — la ayuda corta de este comando.

Sin consulta, se muestra la ayuda corta.

## OPTIONS

- `--all, -a` — con `processes`: listar todos los procesos del sistema
  en lugar de solo los suyos; el servicio concede esta vista únicamente
  a un llamante que posea `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `sysinfo identity` — imprimir la identidad de la máquina y la versión
  del SO.
- `sysinfo ps --all` — listar todos los procesos del sistema.

## EXIT STATUS

- `0` — la consulta fue respondida y mostrada.
- `1` — el servicio rechazó o falló, o el resultado no pudo entregarse.
- `2` — la línea de comandos no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
- `ps`
- `top`
