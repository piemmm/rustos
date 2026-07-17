## NAME

tee — ler da entrada padrão e escrever na saída padrão e em ficheiros

## SYNOPSIS

`tee [option...] [file...]`

## DESCRIPTION

Copia a entrada padrão para a saída padrão e para cada ficheiro
nomeado, para que os dados de um pipeline possam ser vistos e
capturados ao mesmo tempo. Cada ficheiro é criado se estiver ausente e
sobrescrito, salvo se `-a` acrescentar. Um ficheiro que não pode ser
aberto ou escrito é reportado e a execução continua com as restantes
saídas, conforme o modo `--output-error` selecionado.

O TAIRiX não tem `SIGPIPE`: um consumidor que desaparece manifesta-se
como um erro de escrita na saída padrão — a única saída deste comando
que pode ser um pipe — pelo que o «pipe» dos modos GNU significa aqui
exatamente essa saída. Sem `--output-error`, uma saída padrão falhada
para a execução (o equivalente à ferramenta GNU morrer de `SIGPIPE`,
com a razão declarada no erro padrão); com um modo `-nopipe` é tolerada
em silêncio.

O `tee -i` do GNU (ignorar interrupções) não está disponível: o TAIRiX
não tem disposição de sinais por processo para definir. O interruptor
chegará com esse trabalho no núcleo, em vez de ser aceite e ignorado.

## OPTIONS

- `-a, --append` — acrescentar aos ficheiros nomeados; não os
  sobrescrever.
- `-p` — tolerar em silêncio uma saída padrão falhada; o mesmo que
  `--output-error=warn-nopipe`.
- `--output-error[=<mode>]` — como tratar uma saída falhada. Sem valor,
  `warn-nopipe`. Os modos (aceita-se um prefixo não ambíguo): `warn` —
  reportar um erro de escrita em qualquer saída, descartar essa saída e
  continuar; `warn-nopipe` — como `warn`, mas uma saída padrão falhada
  é descartada em silêncio e não afeta o estado de saída; `exit` —
  reportar um erro de escrita em qualquer saída e parar; `exit-nopipe`
  — como `exit`, mas uma saída padrão falhada é descartada em silêncio.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.
- `--` — terminar a análise de opções; cada argumento posterior nomeia
  um ficheiro, e um operando `-` nomeia um ficheiro chamado `-`.

## EXAMPLES

- `ls -l | tee listing.txt` — mostrar a listagem e guardar uma cópia.
- `make 2>&1 | tee -a build.log` — acrescentar uma transcrição da
  compilação enquanto se observa.
- `cat data | tee copy1 copy2 | wc -c` — capturar duas cópias e contar
  os bytes que seguem.

## EXIT STATUS

- `0` — todas as saídas foram servidas até ao fim da entrada (ou a
  ajuda curta pedida foi servida); uma falha da saída padrão tolerada
  por um modo `-nopipe` não muda isto.
- `1` — uma saída falhou de forma que o modo selecionado conta, ou a
  entrada não pôde ser lida.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `cat`
- `head`
- `wc`
