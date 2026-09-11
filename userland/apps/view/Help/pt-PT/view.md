## NAME

view — visualizador gráfico de imagens e documentos

## SYNOPSIS

`view`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que mostra uma imagem ou um
documento. Iniciado com um documento — a partir do gestor de ficheiros, ou
ao abrir uma imagem — mostra esse ficheiro; iniciado sozinho, pede ao
seletor de ficheiros de confiança da sessão para escolher um.

O visualizador não detém qualquer capacidade sobre o sistema de ficheiros:
não pode abrir, listar nem ler nada por si. A sessão navega em seu nome sob
a sua própria identidade, e apenas o ficheiro que o utilizador escolhe lhe é
delegado, de uma só vez e apenas para leitura. O ficheiro nunca é
descodificado dentro do visualizador: os seus bytes são transmitidos a um
processo de trabalho separado que não tem qualquer alcance sobre o sistema
de ficheiros, pelo que um ficheiro malformado ou hostil não consegue
alcançar nada do que o visualizador alcança.

Os formatos suportados são JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO e
RISC OS Sprite. Um ficheiro que o descodificador recusa declara o seu motivo
na janela e no fluxo de erro padrão; a janela nunca é deixada em branco e
nunca é fabricada uma imagem.

Vários documentos ao mesmo tempo são visualizadores separados: comparar duas
imagens lado a lado é abrir a segunda. Fechar uma janela termina o seu
visualizador.

A barra de ferramentas no topo tem, por ordem: reduzir, ampliar, ajustar à
janela, tamanho real, entrada anterior, entrada seguinte, rodar à esquerda,
rodar à direita, espelhar, reproduzir ou pausar uma animação, e o painel de
informação. Um cursor de zoom contínuo fica na sua extremidade final. A
linha de estado em baixo declara o nome do documento, o formato, o tamanho
em píxeis, a entrada mostrada, o comprimento e a ampliação.

Arraste a imagem para se deslocar dentro dela quando for maior do que a
janela; enquanto o for, aparecem barras de deslocamento nas margens da área
de desenho. Rode a roda sobre a imagem para a deslocar. Uma pressão
secundária sobre a imagem abre o menu do visualizador, desenhado pela
sessão.

A transparência é mostrada sobre um padrão de xadrez, para que uma imagem
transparente se leia como transparente e não como a cor que está atrás.

* `+` — ampliar para o passo seguinte
* `-` — reduzir para o passo anterior
* `0` — ajustar toda a imagem à janela
* `1` — tamanho real, um píxel de imagem por píxel de ecrã
* `2` — ajustar a largura da imagem
* `[` / `]` — um quarto de volta à esquerda ou à direita
* `M` — espelhar da esquerda para a direita
* `I` — mostrar ou ocultar o painel de informação
* `Space` — reproduzir ou pausar uma animação
* `O` — escolher outro documento
* `Page Up` / `Page Down` — entrada anterior ou seguinte
* `Home` / `End` — primeira ou última entrada
* teclas de setas — deslocar-se na imagem
* `Escape` — fechar a janela

## OPTIONS

`-h`, `-?`, `--help`
: Escrever esta ajuda na saída padrão e terminar.

## EXIT STATUS

Zero após um fecho limpo. Diferente de zero quando o canal de janela, a
região de trama partilhada ou a sessão foi recusada; o motivo é declarado no
fluxo de erro padrão.
