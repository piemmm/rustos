## NAME

wallpaper — seletor gráfico de fundo de ambiente de trabalho

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Abre uma janela de ambiente de trabalho que oferece os papéis de parede
fornecidos com o sistema, a cor de fundo por trás deles e a forma como o
ambiente de trabalho organiza os ícones no seu painel. Nada muda no ecrã
até que as definições sejam aplicadas.

A grelha apresenta cada papel de parede fornecido como uma miniatura,
mais uma entrada **No wallpaper** (Sem papel de parede) que mostra apenas
a cor de fundo escolhida. Cada miniatura é renderizada com o ajuste
atualmente escolhido, para que uma pré-visualização mostre o que o
ambiente de trabalho fará realmente com essa imagem. Um ficheiro que não
possa ser descodificado apresenta um bloco de substituição marcado com o
seu nome e não é tentado novamente.

As imagens de papel de parede nunca são descodificadas por este
programa. Cada uma é renderizada por um processo isolado (sandbox)
separado que não possui autoridade sobre o sistema de ficheiros, rede ou
execução, pelo que uma imagem malformada não pode comprometer o seletor
ou o ambiente de trabalho.

As linhas de opções abaixo da grelha são:

- **Fit** (Ajuste) — como a imagem é colocada: `fill` (cobrir o ecrã,
  recortando o excesso), `fit` (conter a imagem inteira, cor de fundo
  nas barras), `stretch` (distorcer para o tamanho exato do ecrã),
  `centre` (tamanho nativo, centrado) e `tile` (repetir a partir do
  canto superior esquerdo).
- **Backdrop** (Fundo) — a cor plana apresentada onde o papel de parede
  não chega: `Theme` segue o tema ativo do ambiente de trabalho, e as
  cores nomeadas são fixas. Uma cor já em vigor que não seja uma das
  nomeadas é oferecida sob a sua própria grafia `rrggbb`.
- **Icons** (Ícones) — o lado do painel a partir do qual a grelha de
  ícones do ambiente de trabalho cresce.
- **Sort** (Ordenação) — a ordem pela qual os ícones da pasta do
  ambiente de trabalho são listados.

A janela é controlada pelo teclado. `Tab` e `Shift-Tab` movem o foco para
a frente e para trás através da grelha, das linhas de opções e dos
botões. As teclas de seta movem-se dentro da grelha de miniaturas ou
alteram a opção focada. `Enter` ativa o botão focado e `Escape` fecha a
janela sem aplicar.

A aplicação envia as definições escolhidas para a sessão de ambiente de
trabalho, que decide se as adota, redesenha o painel e as guarda para o
próximo início de sessão. Este programa nunca escreve as definições por
si mesmo. O resultado é reportado na linha de estado por baixo das
linhas de opções: aplicado, recusado com o motivo da sessão ou nenhuma
sessão de ambiente de trabalho à escuta. Uma recusa mantém a janela
aberta com as escolhas intactas.

Apenas é oferecido o armazenamento de papéis de parede fornecidos; uma
imagem noutro local do sistema não pode ser escolhida a partir desta
janela. Cliques do ponteiro não selecionam nada.

## EXIT STATUS

Zero após um fecho limpo, incluindo quando as definições foram
recusadas. Diferente de zero quando a janela não pôde ser aberta, a
região de moldura partilhada foi recusada ou o canal da janela foi
perdido; o motivo é indicado no fluxo de erro padrão.

## ENVIRONMENT

`HOME` indica o diretório pessoal do utilizador, sob o qual
`Settings/Pinboard/pinboard.conf` é lido no arranque para que a janela
se abra com as definições que estão em vigor. Esse documento é escrito
pela sessão de ambiente de trabalho, nunca por este programa. Sem
`HOME`, a janela abre-se com os valores predefinidos.

## SEE ALSO

`files`, `viewer`
