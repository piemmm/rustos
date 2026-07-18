## NAME

sysmon — observar ao vivo a memória e a carga do núcleo

## SYNOPSIS

`sysmon [-d seg.décimos] [-h | -?]`

## DESCRIPTION

Mostra em ecrã inteiro, ao vivo, a memória e a carga do núcleo através
da API de informação do sistema: memória física, o heap do núcleo, a
banda de pressão de memória com o seu histórico, o registo das caches
recuperáveis, o nível comprimido `ramzip`, o total de memória fixada, a
carga por CPU e um censo de processos. A ferramenta mantém-se utilizável
sob carga deliberada e repousa entre atualizações quando o sistema está
ocioso.

No arranque, o monitor fixa a sua própria memória (`mem_pin`, que
requer `CAP_MEM_PIN`) para nunca parar nos seus próprios carregamentos
de página sob a mesma pressão que observa. Uma fixação recusada é
comunicada na linha de título e a sessão continua sem fixação — a
fixação é acessória, nunca fatal.

O ecrã atualiza-se a cada intervalo (3,0 segundos salvo indicação de
`-d`), e `r` atualiza-o de imediato. O monitor não aceita operandos:
controla-se com teclas dentro da sessão.

- `q` — sair.
- `p` — alternar o painel de detalhe: caches recuperáveis, nível
  comprimido, carga por CPU, linhas de interrupção, processos.
- `r` — atualizar agora.
- `+` / `-` — alongar / encurtar o intervalo em um segundo, entre 0,1
  e 60 segundos.
- Cima/Baixo, PgCima/PgBaixo, Início/Fim — deslocar o painel.
- `h`, `?` — mostrar ou ocultar o resumo de teclas.

Seis linhas de resumo precedem o painel de detalhe: o título (tempo de
atividade, médias de carga e estado de fixação); os valores de memória
em MiB com o total fixado; a banda de pressão com o seu indicador,
valores de livre/reserva e contadores de entrada; o histórico de bandas
(um glifo por atualização: `.` normal, `-` leve, `=` moderada, `#`
severa, `!` crítica); a linha global de CPU; e o censo de tarefas.

Cada valor viaja pela API de informação do sistema — não há `/proc`.
As consultas de estatísticas do núcleo requerem `CAP_SYSINFO_KERNEL`,
e o censo de todos os processos `CAP_SYSINFO_GLOBAL`: a quem falte uma
delas é explicada a recusa desse painel enquanto o resto da sessão
continua. A lista interativa completa de processos é tarefa do `top`;
o painel de processos mostra aqui apenas o censo e os maiores
consumidores por `%CPU` e por memória.

## OPTIONS

- `-d, --delay <seconds>` — o intervalo entre atualizações
  automáticas, em segundos com fração opcional (só se conserva o
  primeiro dígito decimal, os décimos): `sysmon -d 1.5` atualiza a
  cada 1,5 segundos. Predefinição 3,0. O GNU `top` aceita um intervalo
  zero e atualiza tão depressa quanto pode; o TAIRiX nunca roda em
  vazio, pelo que um zero é elevado ao mínimo de 0,1 s.
- `-h, -?` — mostrar a ajuda curta deste comando e sair. Dentro de uma
  sessão em curso, as mesmas teclas alternam o resumo de teclas.

## EXIT STATUS

- `0` — a sessão terminou com `q`, ou foi mostrada a ajuda curta.
- `1` — o terminal falhou; a razão é escrita no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda curta (uma etiqueta
  BCP-47 como `pt-PT`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
