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
como são; qualquer outro byte é representado por um ponto. O conteúdo
mostrado é limitado ao início do ficheiro.

Prima `Enter` para pedir outro ficheiro. Cancelar o seletor deixa o
visualizador aberto com um aviso. Fechar a janela a partir do ambiente
de trabalho termina o visualizador.

## EXIT STATUS

Zero após um fecho limpo; diferente de zero quando o canal de janela
ou a região de fotogramas partilhada foi recusada (o motivo é indicado
no fluxo de erro padrão).
