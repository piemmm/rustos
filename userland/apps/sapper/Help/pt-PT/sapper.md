## NAME

sapper — limpar o campo de minas sem accionar nenhuma

## SYNOPSIS

`sapper`

## DESCRIPTION

Abre uma janela com uma grelha de casas cobertas. Algumas escondem uma mina.
Cada casa que descobrir e não a esconda indica quantas das suas oito vizinhas a
escondem, e esses números bastam para deduzir onde estão as minas. Descubra todas
as casas sem mina e ganhou; descubra uma com mina e a partida termina.

A primeira casa que descobrir é sempre segura, tal como as oito à sua volta, pelo
que uma jogada de abertura nunca pode perder e abre sempre uma zona a partir da
qual raciocinar.

Clique numa casa coberta para a descobrir. Com o botão secundário coloca uma
bandeira, outra vez um ponto de interrogação se estiverem activos, e outra vez
apaga-a. Uma casa com bandeira está protegida: clicar nela não faz nada.

Uma vez aberta uma casa, o seu número pode trabalhar por si. Clique num número
cujas bandeiras já correspondam e todas as vizinhas restantes são descobertas de
uma vez. Clique nele com o botão secundário quando as suas vizinhas cobertas
forem exactamente tantas quantas o seu número e todas são marcadas de uma vez. O
botão do meio faz o mesmo que o primeiro, a partir de qualquer ponto da casa.

O contador à esquerda mostra quantas minas faltam encontrar, menos as bandeiras
colocadas; fica negativo se colocar mais bandeiras do que há minas. O relógio à
direita arranca na sua primeira casa descoberta e pára no fim da partida. O botão
entre ambos inicia uma nova partida, e a sua cara diz como acabou a anterior.

As setas movem um anel pela grelha. `Space` descobre a casa que contém, ou
acorda-a se já estiver aberta. `F` marca a casa, `Shift+F` marca todas as suas
vizinhas cobertas, `N` inicia uma nova partida, e `1`, `2` e `3` escolhem as
grelhas principiante, intermédia e perita. As mesmas escolhas, e a definição dos
pontos de interrogação, estão no menu da barra de ícones do jogo.

O seu melhor tempo em cada uma das três grelhas padrão é recordado entre sessões.
Uma grelha do seu próprio tamanho não guarda nenhum, porque duas grelhas dessas
nunca são a mesma partida.

O jogo é lançado a partir da Biblioteca de programas do ambiente de trabalho, em
Games, ou pelo nome a partir de uma shell. Exige uma sessão gráfica em curso: sem
ela o canal de janela é inalcançável e o jogo comunica a recusa no fluxo de erro
padrão e termina.

## EXIT STATUS

Zero após um fecho limpo; diferente de zero quando o canal de janela ou a região
de trama partilhada foi recusada (o motivo é indicado no fluxo de erro padrão).
