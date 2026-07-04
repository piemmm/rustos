## NAME

ps — listar procesos

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

Lista los procesos a través de la API de información del sistema. Por
defecto solo se listan los procesos del llamante; el servicio aplica
cada ámbito de consulta según la identidad del llamante atestiguada por
el núcleo, y ningún camino elude ese control.

Cada proceso se imprime como una fila bajo una cabecera de columnas: el
identificador del proceso (`PID`), el del proceso padre (`PPID`), los
identificadores de usuario y de grupo propietarios (`UID`, `GID`), el
estado de planificación (`S`), la CPU en la que el proceso se ejecutó
por última vez (`CPU`), y el nombre del comando (`NAME`).

`ps` no acepta operandos.

## OPTIONS

- `-e, -A, --all` — listar todos los procesos del sistema en lugar de
  solo los del llamante; el servicio concede esta vista únicamente a un
  llamante que posea `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `ps` — listar sus propios procesos.
- `ps -e` — listar todos los procesos del sistema.

## EXIT STATUS

- `0` — la lista fue escrita.
- `1` — el servicio rechazó o falló, o la lista no pudo entregarse.
- `2` — la línea de comandos no fue comprendida.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

- `man`
- `top`
- `sysinfo`
