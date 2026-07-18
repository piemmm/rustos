## NAME

sleep — pausar durante a soma de intervalos de tempo

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

Pausa durante a soma dos intervalos indicados e depois termina.

Cada `NUMBER` é um valor de vírgula flutuante; um `SUFFIX` de uma só letra
escala-o: `s` para segundos (predefinição), `m` para minutos, `h` para
horas e `d` para dias. Vários operandos são somados, pelo que
`sleep 1m 30s` pausa noventa segundos. `inf` (ou `infinity`) pausa até que
o processo seja terminado.

Ao contrário da temporização própria de uma shell, `sleep` dorme fora do
processador: a tarefa fica estacionada até o intervalo decorrer e nunca
mantém um núcleo a girar em vazio.

Um valor negativo, um `nan`, um sufixo desconhecido ou caracteres
adicionais após o número é um `invalid time interval`. Não dar qualquer
operando é um `missing operand`.

Este comando não imprime uma versão do sistema; o TAIRiX não tem tal
cadeia, por isso — ao contrário do GNU `sleep` — não tem a opção
`--version`.

## OPTIONS

- `-h, -?` — mostrar a ajuda breve deste comando.
- `--` — terminar a análise de opções; qualquer argumento posterior é um
  operando.

## EXAMPLES

- `sleep 5` — pausar cinco segundos.
- `sleep 1.5h` — pausar noventa minutos.
- `sleep 1m 30s` — pausar noventa segundos (os operandos são somados).
- `sleep inf` — pausar até que o processo seja terminado.

## EXIT STATUS

- `0` — o intervalo decorreu, ou foi escrita uma ajuda breve pedida.
- `1` — a escrita da ajuda breve falhou.
- `2` — a linha de comandos não foi compreendida (uma opção desconhecida,
  um operando em falta ou um intervalo de tempo inválido).

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda breve (uma etiqueta BCP-47 como
  `fr-FR`).

## SEE ALSO

- `top`
