## NAME

desktop — iniciar la sesión gráfica de escritorio

## SYNOPSIS

`desktop`

## DESCRIPTION

Inicia la sesión gráfica de escritorio en el puesto de esta máquina: el
comando adquiere el arrendamiento exclusivo de pantalla y entrada del
puesto, se conecta al servicio de pantalla y ejecuta el escritorio
compositado — el gestor de ventanas y la barra de tareas — hasta que la
sesión termina. El comando retorna cuando la sesión de escritorio
termina.

El mismo escritorio se inicia automáticamente tras la autenticación: un
inicio de sesión gráfico (`os.loginType`) es el valor por omisión en una
máquina que pueda ejecutarlo. Este comando lo inicia bajo demanda desde
un shell de texto.

Cuando no hay ningún servicio de pantalla en ejecución, o cuando otra
sesión ya posee el puesto, el comando falla escribiendo su motivo en la
salida de error estándar — nunca desplaza una sesión en curso.

## OPTIONS

- `-h, -?` — mostrar la ayuda corta de este comando.

## EXAMPLES

- `desktop` — iniciar la sesión de escritorio.

## EXIT STATUS

- `0` — se sirvió la ayuda corta.
- `2` — la línea de comandos no se entendió.
- cualquier otro código distinto de cero — la sesión no pudo iniciarse
  (sin puesto, sin servicio de pantalla) o terminó (se perdió el
  arrendamiento del puesto); el motivo se escribe en la salida de error
  estándar.

## ENVIRONMENT

- `LANG` — la configuración regional preferida para la ayuda corta (una
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `configure`
- `man`
