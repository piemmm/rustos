## NAME

top — observar a lista de processos ao vivo

## SYNOPSIS

`top [-d secs.tenths] [-h | -?]`

## DESCRIPTION

Mostra uma vista ao vivo, em ecrã inteiro, da lista de processos
através da API de Informação do Sistema, no espírito do `top` GNU.
Começa nos processos do próprio chamador; a vista de todo o sistema só
é concedida pelo serviço a um chamador que detenha
`CAP_SYSINFO_GLOBAL`.

O ecrã atualiza-se a cada intervalo de atraso (3,0 segundos salvo se
`-d` o mudar), e `r` atualiza-o imediatamente.

O visualizador não aceita operandos: controla-se com teclas premidas
dentro da sessão.

- `q` — sair.
- `a` — alternar entre os seus próprios processos e a vista de todo o
  sistema. Se o serviço recusar a vista de todo o sistema (exige
  `CAP_SYSINFO_GLOBAL`), o visualizador fica nos seus próprios
  processos e a linha de estado diz porquê; a sessão continua.
- `r` — atualizar a listagem.
- Cima/Baixo, PageUp/PageDown, Home/End — mover a seleção.
- `h`, `?` — alternar a sobreposição de teclas na sessão.

Quatro linhas de resumo precedem a lista: o tempo de atividade, a
contagem de utilizadores com sessão e as médias de carga de 1/5/15
minutos; o censo de tarefas por estado; a repartição de utilização
`%Cpu(s)`; e os números de memória em MiB. A linha de memória exige
`CAP_SYSINFO_KERNEL` — um chamador sem ela vê a recusa explicada e a
sessão continua.

A linha `%Cpu(s)` mostra a fração do último intervalo que todos os CPU
juntos passaram ocupados (a correr tarefas) e inativos. O TAIRiX
contabiliza apenas tempo ocupado e inativo, pelo que onde o `top` GNU
divide a fração ocupada em user/system/nice/iowait, esta linha mostra
deliberadamente os dois números reais.

As filas são ordenadas por `%CPU`, o maior consumidor primeiro, e
carregam:

- `PID` — o id numérico do processo.
- `USER` — o nome de utilizador da conta dona, resolvido do diretório
  de contas do sistema; o uid numérico substitui-o quando o nome não
  pode ser resolvido.
- `SIZE` — a memória mapeada no espaço de endereçamento do processo
  (imagem, pilha e heap por igual).
- `S` — a letra de estado: `R` a correr (verde), `r` executável, à
  espera de um CPU (ciano), `S` a dormir, `T` parado (amarelo), `Z`
  zombie (magenta). As cores só aparecem num terminal a cores; a letra
  transporta sempre o estado.
- `%CPU` — a fração de CPU no intervalo desde a atualização anterior.
- `WCPU` — a fração de CPU ponderada (suavizada exponencialmente)
  através das atualizações, mais estável do que a coluna instantânea.
- `TIME+` — o tempo de CPU acumulado, como
  `minutos:segundos.centésimos`.
- `COMMAND` — o nome do processo.

## OPTIONS

- `-d, --delay <seconds>` — o intervalo entre atualizações automáticas,
  em segundos com fração opcional (só o primeiro dígito fracionário,
  décimos, é conservado): `top -d 1.5` atualiza a cada 1,5 segundos. A
  omissão é 3,0. O `top` GNU aceita um atraso zero e atualiza o mais
  depressa que consegue; o TAIRiX nunca faz espera ativa, pelo que um
  zero é fixado ao mínimo de 0,1 s.
- `-h, -?` — mostrar a ajuda curta deste próprio comando e sair. Dentro
  de uma sessão as mesmas teclas alternam a sobreposição de teclas.

## EXIT STATUS

- `0` — a sessão terminou com `q`, ou a ajuda curta foi mostrada.
- `1` — o serviço ou o terminal falhou; a razão é impressa no erro
  padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
