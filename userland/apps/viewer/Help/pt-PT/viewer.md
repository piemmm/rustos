## NAME

viewer — visualizador gráfico de ficheiros só de leitura

## SYNOPSIS

`viewer`

## DESCRIPTION

Abre uma janela do ambiente de trabalho e pede de imediato ao seletor
de ficheiros de confiança da sessão que escolha um ficheiro. O
visualizador não detém qualquer capacidade de sistema de ficheiros:
por si só não pode abrir, listar nem ler nada. A sessão navega em nome
do visualizador sob a sua própria identidade, e apenas o ficheiro
escolhido pelo utilizador é delegado ao visualizador — de utilização
única e só de leitura.

O conteúdo do ficheiro escolhido é mostrado como texto simples a
partir do topo da janela. Os caracteres imprimíveis são mostrados tal
como são; qualquer outro byte é representado por um ponto, de modo que
o conteúdo binário pareça obviamente saneado. O conteúdo mostrado é
limitado ao início do ficheiro.

A janela é controlada pelo rato. Clique no botão **Open…** (Abrir…) no
cabeçalho para pedir outro ficheiro. Arraste o cursor da barra de
deslocamento para cima ou para baixo para percorrer um ficheiro longo,
clique na calha acima ou abaixo do cursor para avançar a página,
clique nos botões de extremidade para avançar uma linha ou rode a roda
sobre a janela para deslocar. Cancelar o seletor deixa o visualizador
aberto com un aviso; fechar a janela a partir do ambiente de trabalho
termina o visualizador.

O teclado é uma via secundária para as mesmas ações: `Enter` pede outro
ficheiro, as teclas de seta avançam uma linha, Page Up/Page Down
avançam uma página e Home/End saltam para o topo ou para o fim.

## EXIT STATUS

Zero após um fecho limpo; diferente de zero quando o canal de janela
ou a região de fotogramas partilhada foi recusada (o motivo é indicado
no fluxo de errore padrão).
