## NAME

elsh — el intérprete de comandos de RustOS

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Ejecuta un intérprete de comandos interactivo — un bucle
leer-evaluar-imprimir sobre los flujos estándar heredados. Una palabra
de comando escrita se resuelve primero contra los comandos integrados
del intérprete, luego en la tienda de aplicaciones del sistema
(`/System/Apps`), y después en los directorios de la variable `PATH`;
la tienda se busca antes que `PATH`, de modo que `PATH` nunca puede
ocultar un comando del sistema. Una palabra no resuelta sale con
`127`; un paquete resuelto pero no ejecutable sale con `126`.

Los comandos integrados:

- `cd <path>`, `pwd` — cambiar e imprimir el directorio de trabajo.
- `echo ...` — imprimir sus operandos.
- `export NAME=value`, `unset NAME` — editar el entorno exportado.
- `jobs`, `fg`, `bg` — control de tareas.
- `ulimit` — leer e imponer límites de recursos.
- `elevate` — ejecutar un comando re-autenticado a través del
  supervisor de inicio de sesión de la consola.
- `help` — listar los comandos integrados.
- `exit [code]` — terminar la sesión.

El intérprete no acepta operandos: la ejecución de guiones aún no forma
parte de su gramática.

## OPTIONS

- `-h, -?` — mostrar la ayuda corta de este comando y salir.

## EXIT STATUS

- El código del comando integrado `exit`, o `0` cuando el flujo de
  entrada termina (o se mostró la ayuda corta).
- `2` — la invocación no fue comprendida.

## ENVIRONMENT

- `PATH` — los directorios buscados después de la tienda de
  aplicaciones del sistema.
- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`), exportada a cada comando lanzado.

## SEE ALSO

- `man`
