## NAME

settings — configurar o ambiente de trabalho e esta máquina

## SYNOPSIS

`settings`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que lista cada categoria de definições
deste sistema: o que a máquina é, o aspeto do ambiente de trabalho, o ecrã, a
rede, os dispositivos com que é conduzida, as contas que a usam e os volumes
que contém. Escolher uma categoria na barra lateral mostra o seu painel.

O Settings não detém autoridade própria. Cada alteração é ou um pedido à sessão
do ambiente de trabalho, dona das definições do utilizador, ou uma execução
reautenticada do comando que já escreve esse armazém; nada aqui pode elevar um
privilégio.

Uma categoria que este sistema não consegue servir di-lo claramente e nomeia o
que teria de existir para o conseguir. Uma categoria que consegue servir, cujos
controlos esta versão ainda não desenha, indica em vez disso onde a definição é
lida ou definida. Um controlo que nada alteraria nunca é mostrado.

Escreva no campo de pesquisa acima da barra lateral para a filtrar às
categorias e definições que uma palavra alcança. `Tab` e `Shift+Tab` movem o
foco entre o campo de pesquisa, o percurso, a barra lateral e o painel; `Up` e
`Down` percorrem a barra lateral e `Enter` abre a linha. Uma janela demasiado
estreita abandona a barra lateral, e a primeira migalha do percurso lista então
as categorias.

É lançado a partir da linha *Settings…* do menu de sistema do ambiente de
trabalho, da Biblioteca de programas, ou pelo nome a partir de uma shell.
Exige uma sessão gráfica em execução: sem ela o canal de janela é inalcançável
e comunica a recusa no fluxo de erro padrão e termina.

## EXIT STATUS

Zero após um fecho limpo; diferente de zero quando o canal de janela ou a
região de fotogramas partilhada foi recusada (o motivo é indicado no fluxo de
erro padrão).
