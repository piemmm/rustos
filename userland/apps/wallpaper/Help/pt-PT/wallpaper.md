## NAME

wallpaper — seletor gráfico de fundo de ambiente de trabalho

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Abre uma janela de ambiente de trabalho que oferece os papéis de parede
fornecidos com o sistema, a cor de fundo por trás deles e a forma como o
ambiente de trabalho organiza os ícones no seu painel. Nada muda no ecrã
até que as definições sejam aplicadas.

A janela é controlada pelo rato. Uma grande pré-visualização no topo
mostra o papel de parede selecionado tal como o ambiente de trabalho o
desenhará, com a cor de fundo escolhida onde quer que a imagem não
chegue. Por baixo, a galeria apresenta cada papel de parede fornecido
como um bloco: clique num para o selecionar e a pré-visualização segue-se
imediatamente. O bloco **No wallpaper** (Sem papel de parede), sempre em
primeiro lugar, mostra apenas a cor de fundo escolhida.

A galeria permite o deslocamento quando contém mais blocos do que a
janela apresenta. Rode a roda em qualquer ponto da janela, arraste o
indicador da barra de deslocamento no bordo posterior, ou clique na
calha acima ou abaixo do indicador para se deslocar uma página de cada
vez.

Ao lado da pré-visualização encontram-se quatro definições, cada uma numa
lista pendente. Clique numa para a abrir e clique numa opção para a
escolher:

- **Fit** (Ajuste) — como a imagem é colocada: `fill` (cobrir o ecrã,
  recortando o excesso), `fit` (conter a imagem inteira, cor de fundo
  nas barras), `stretch` (distorcer para o tamanho exato do ecrã),
  `centre` (tamanho nativo, centrado) e `tile` (repetir a partir do
  canto superior esquerdo).
- **Backdrop** (Fundo) — a cor plana apresentada onde o papel de parede
  não chega: `Theme` segue o tema ativo do ambiente de trabalho, e as
  cores nomeadas são fixas. Uma cor já em vigor que não seja uma das
  nomeadas é oferecida sob a sua própria grafia `rrggbb`.
- **Icons** (Ícones) — o canto do painel a partir do qual a grelha de
  ícones do ambiente de trabalho cresce.
- **Sort** (Ordenação) — a ordem pela qual os ícones da pasta do
  ambiente de trabalho são listados.

A pré-visualização é um modelo à escala do seu ecrã: tem a mesma forma
do visor e mostra a imagem, o fundo e o ajuste selecionados
exatamente como o ambiente de trabalho os irá mostrar. O que vê na
pré-visualização é o que obtém.

As imagens de papel de parede nunca são descodificadas por este
programa. Cada uma é renderizada por um processo isolado (sandbox)
separado que não possui autoridade sobre o sistema de ficheiros, rede ou
execução, pelo que uma imagem malformada não pode comprometer o seletor
ou o ambiente de trabalho. Um ficheiro que não possa ser descodificado é
marcado como `unreadable` no seu bloco e não é tentado novamente.

O teclado alcança tudo o que o rato faz. `Tab` e `Shift-Tab` movem o foco
para a frente e para trás através da galeria, das quatro definições e
dos dois botões. As teclas de seta movem-se dentro da galeria, ou abrem
a lista da definição focada e movem-se dentro dela. `Enter` aplica, ou
ativa o botão focado, e `Escape` fecha a janela sem aplicar.

A aplicação envia as definições escolhidas para a sessão de ambiente de
trabalho, que decide se as adota, redesenha o painel e as guarda para o
próximo início de sessão. Este programa nunca escreve as definições por
si mesmo. O resultado é reportado ao lado dos botões: aplicado, recusado
com o motivo da sessão ou nenhuma sessão de ambiente de trabalho à
escuta. Uma recusa mantém a janela aberta com as escolhas intactas.

Apenas é oferecido o armazenamento de papéis de parede fornecidos; uma
imagem noutro local do sistema não pode ser escolhida a partir desta
janela.

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
