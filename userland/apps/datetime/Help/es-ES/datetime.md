## NAME

datetime — ajustar la fecha y la hora de la máquina

## SYNOPSIS

`datetime`

## DESCRIPTION

Abre una ventana de escritorio que muestra el reloj de la máquina en seis
campos editables — año, mes y día en la primera fila, hora, minuto y
segundo en la segunda — y ajusta el reloj a lo que indican. Nada cambia
hasta que se pulsa **Set**.

La lectura es UTC. TAIRiX no mantiene ningún desplazamiento de zona
horaria, así que no hay hora local que mostrar ni que introducir.

Normalmente se llega a la ventana desde el propio menú del reloj del
escritorio: pulsar el reloj en la barra de iconos y elegir **Set Date &
Time…**. Ajustar el reloj requiere una autoridad que una sesión de
escritorio no tiene, de modo que el escritorio pide una cuenta que sí la
tenga, y esta aplicación se inicia como esa cuenta una vez aceptada la
contraseña.

Pulsar un campo para escribir en él, o `Tab` para pasar al siguiente.
Solo se aceptan dígitos, con un `-` inicial permitido en el año para una
fecha anterior al año 1. `Enter` ajusta el reloj; `Escape` cierra la
ventana.

Todos los campos se comprueban antes de ajustar nada, y el primer fallo
se indica en la ventana en lugar de corregirse en silencio: un mes fuera
de 1 a 12, una hora fuera de 0 a 23, un minuto o segundo fuera de 0 a 59,
o un día que no existe en el mes y el año introducidos — el 31 de abril,
o el 29 de febrero fuera de un año bisiesto. No se ajusta nada cuando un
campo se rechaza.

Las fechas anteriores a 1970 y muy posteriores a 2038 son entradas
ordinarias. El reloj es un valor de 64 bits con signo, así que ninguna de
las dos es un límite.

Si el reloj de la máquina no se ha ajustado nunca desde que arrancó, los
campos se abren **vacíos** y la ventana lo dice. No se rellenan con la
época Unix, que sería una fecha que la máquina nunca afirmó.

Si la cuenta con la que se ejecuta esta aplicación no puede ajustar el
reloj, el intento se rechaza, la ventana lo dice y el reloj queda
exactamente como estaba. La razón se escribe además en el flujo de error
estándar. La aplicación sigue funcionando: un ajuste rechazado es una
respuesta, no un fallo del programa.

## EXIT STATUS

Cero tras un cierre limpio, incluso cuando un ajuste fue rechazado. No
cero cuando no se pudo abrir la ventana, se rechazó la región de trama
compartida o se perdió el canal de la ventana; la razón se indica en el
flujo de error estándar.

## SEE ALSO

`sysinfo`, `uptime`
