## NAME

fstree — o gestor de ficheiros em árvore de ecrã inteiro

## SYNOPSIS

`fstree [diretório]`

## DESCRIPTION

Percorre o sistema de ficheiros numa sessão de ecrã inteiro guiada pelo
teclado: um painel com a árvore de diretórios à esquerda e um painel de
ficheiros à direita que lista as entradas do diretório selecionado com os
seus tamanhos e datas de modificação. A sessão começa em `diretório`
(a vista raiz `/` por omissão).

A árvore é lida preguiçosamente: o conteúdo de um diretório só é obtido
quando é mostrado ou expandido pela primeira vez, pelo que percorrer um
volume enorme custa apenas os diretórios realmente abertos. Um diretório
que o chamador não pode listar é recusado no local — o erro aparece na
linha de mensagens e a vista anterior mantém-se; nada é fabricado.

Teclas:

- `Cima`/`Baixo` ou `k`/`j` — mover o cursor do painel ativo. Mover o
  cursor da árvore lista o diretório recém-selecionado no painel de
  ficheiros.
- `Esquerda`/`Direita` ou `h`/`l` — recolher/expandir a linha da árvore
  sob o cursor.
- `Enter` — na árvore, alterna a expansão; no painel de ficheiros, desce
  ao diretório selecionado (ambos os painéis acompanham).
- `Tab` — trocar o painel ativo.
- `s` — abrir o menu de ordenação: `n` nome, `e` extensão, `s` tamanho,
  `m` data de modificação, `r` inverter o sentido, `Esc` cancela. Os
  diretórios agrupam-se sempre antes dos ficheiros.
- `c` — copiar a entrada selecionada: uma linha pede o destino. Um
  destino relativo cai no diretório listado; um destino que é um
  diretório existente recebe a cópia lá dentro, com o nome da origem.
  Um diretório é copiado com todo o seu conteúdo. Copiar uma entrada
  sobre si própria ou um diretório para dentro da sua própria subárvore
  é recusado antes de qualquer escrita.
- `m` — mover a entrada selecionada, com a mesma pergunta de destino.
  Dentro do mesmo volume a mudança é uma renomeação atómica; entre
  volumes a entrada é copiada e a origem depois removida.
- `r` — renomear a entrada selecionada no local: a linha vem
  pré-preenchida com o nome atual.
- `d` — apagar a entrada selecionada após uma confirmação; só `y`
  prossegue. Apagar um diretório remove todo o seu conteúdo, e a
  confirmação di-lo.
- `M` — criar um diretório no diretório listado; o nome é pedido.
- `a` — editar os bits de permissão da entrada selecionada: uma linha
  octal pré-preenchida com o modo atual. Enter aplica (só o proprietário
  pode alterá-lo — o núcleo recusa qualquer outro), Esc cancela.
- `t` — marcar ou desmarcar a entrada selecionada do painel de
  ficheiros e descer uma linha; pressionar repetidamente marca uma
  série. As entradas marcadas trazem um `*`.
- `T` — marcar por padrão: um glob (`*`, `?`, `[...]`) comparado com
  os nomes visíveis; cada correspondência junta-se ao conjunto
  marcado.
- `i` — inverter as marcas sobre as entradas visíveis.
- `C` — limpar todas as marcas.
- `u` — contar o uso do disco sob o diretório focado: ficheiros,
  bytes e diretórios, percorridos de forma incremental em segundo
  plano. `Esc` cancela, mantendo os números contados até aí.
- `v` — aplanar o ramo sob o diretório focado: uma lista de cada
  ficheiro abaixo dele, preenchida página a página (`Espaço` carrega
  a seguinte). Na vista, `t`/`T`/`i`/`C` marcam as suas linhas,
  `c`/`m`/`d` executam operações em lote sobre o conjunto marcado e
  `Esc` regressa aos painéis. As linhas são nomeadas relativamente ao
  ramo aplanado.
- `.` — mostrar/ocultar as entradas ocultas (nomes com ponto) em ambos os
  painéis.
- `?` — mostrar esta ajuda sobre os painéis; qualquer tecla fecha-a.
- `q` — sair, restaurando o terminal.

Enquanto houver entradas marcadas, `c`, `m` e `d` atuam sobre todo o
conjunto marcado em vez da seleção: `c`/`m` pedem um diretório de
destino existente onde as entradas caem, e `d` confirma a eliminação
em lote. As entradas são processadas por ordem de marcação; uma falha
nunca trava as restantes, o relatório final conta o que teve sucesso e
um ecrã de relatório nomeia cada falha — um lote nunca fica
silenciosamente parcial. As entradas com sucesso são desmarcadas; as
falhas ficam marcadas para nova tentativa.

Quando uma cópia ou mudança iria sobrescrever um ficheiro existente, a
sessão pergunta por ficheiro: `o` sobrescreve, `s` salta (uma origem
saltada fica no seu lugar) e `c` cancela os passos restantes — num
lote, cancelar abandona todas as entradas restantes — o que
já foi aplicado permanece, e o relatório final diz o que aconteceu. Uma
falha a meio da cópia remove o destino meio escrito e mostra o erro do
núcleo; nada se faz passar por uma cópia completa. Cada operação é
autorizada pelo núcleo — uma recusa aparece tal e qual na linha de
mensagens sem que nada mude.

A linha de estado mostra o caminho listado, o número de entradas
visíveis, a ordem de ordenação, os bytes livres/totais do volume
subjacente (quando o serviço de informação do sistema os pode reportar),
se as entradas ocultas estão visíveis e — enquanto algo estiver
marcado — o número de entradas marcadas com o seu total de bytes. Um
ficheiro cujo formato de
armazenamento não guarda data de modificação mostra `-` na coluna da
data.

A pesquisa e os visualizadores de texto/hexadecimal/
desassemblagem chegam em fases posteriores do plano da ferramenta.

## OPTIONS

- `directory` — o diretório onde a sessão começa; o valor por omissão é a
  vista raiz `/`.
- `-h`, `-?` — imprimir a forma curta deste documento e sair.

## EXIT STATUS

- `0` — a sessão terminou com o `q` do utilizador.
- `1` — o diretório inicial não pôde ser listado, ou o caminho do
  terminal falhou.
- `2` — os argumentos não puderam ser compreendidos.

## SEE ALSO

ls, cp, mv, rm, mkdir, chmod, du, df, find
