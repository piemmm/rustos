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
sesión del próximo arranque).

El conjunto de claves es cerrado: una clave desconocida, o un valor
fuera del conjunto de una clave, se rechaza indicando las opciones
válidas y no cambia nada. Cambiar un ajuste reescribe el almacén en su
forma canónica y requiere acceso de escritura a `/System/Settings`: una
cuenta ordinaria puede leer los ajustes pero no cambiarlos.

- `os.loginType` — `text` o `graphical`: qué tipo de sesión ofrece por
  defecto el servicio de inicio de sesión. `text` (el valor por
  defecto) conserva la pregunta de sesión con texto por defecto;
  `graphical` inicia directamente la sesión de escritorio tras la
  autenticación cuando hay un escritorio instalado, y recurre al texto
  cuando no lo hay.

## OPTIONS

- `-h, -?` — mostrar la ayuda breve de esta orden.

## EXAMPLES

- `configure` — listar todos los ajustes.
- `configure os.loginType` — mostrar el tipo de sesión por defecto.
- `configure os.loginType graphical` — arrancar en el inicio de sesión
  gráfico.

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
