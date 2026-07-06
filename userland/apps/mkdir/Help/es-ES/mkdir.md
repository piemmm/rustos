## NAME

mkdir — crear directorios

## SYNOPSIS

`mkdir [-pv] [--] directorio...`

## DESCRIPTION

Crea cada directorio operando, en orden. Sin `-p`, el directorio padre
de cada operando debe existir ya y el operando mismo no debe existir;
el primer fallo detiene la ejecución antes de cualquier operando
posterior.

Con `-p` se crea primero cada ancestro que falte, del más externo al
más interno, y un operando (o ancestro) que ya exista como directorio
no es un error. Un ancestro que exista como fichero sigue fallando:
nada se reemplaza nunca en silencio.

La opción `-m`/`--mode` de GNU `mkdir` aún no se acepta: los
directorios se crean con el modo predeterminado del sistema de ficheros
hasta que llegue el mecanismo para fijar modos; la opción llegará con
él en lugar de ignorarse. `--` termina el análisis de opciones: cada
argumento posterior es una ruta.

## OPTIONS

- `-p, --parents` — crear los directorios padres que falten; un
  operando que ya es un directorio no es un error.
- `-v, --verbose` — informar de cada directorio creado como
  `mkdir: created directory 'dir'`.
- `-h, -?` — mostrar la ayuda corta de esta orden (también `--help`).

## EXAMPLES

- `mkdir Notes` — crear un directorio en el directorio actual.
- `mkdir -p Projects/os/build` — crear toda la cadena, omitiendo las
  partes que ya existen.
- `mkdir -pv Home:/tools/bin` — crear bajo una raíz de alias,
  informando de cada directorio nuevo.

## EXIT STATUS

- `0` — todos los directorios se crearon (o, con `-p`, ya existían).
- `1` — un fallo del sistema de ficheros o de la salida; la razón se
  imprime en la salida de error.
- `2` — la línea de órdenes no se entendió.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `es-ES`).

## SEE ALSO

rmdir, rm, ls
