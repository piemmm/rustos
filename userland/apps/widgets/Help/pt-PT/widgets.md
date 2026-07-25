## NAME

widgets — galeria de componentes Reactive Alloy

## SYNOPSIS

`widgets`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que demonstra cada controlo gráfico
partilhado do TAIRiX no seu próprio separador: botões, seletores, controlos de
valor, campos de texto, controlos de escolha, coleções, barras, superfícies de
retorno e controlos de janela. Cada separador mostra várias variantes da sua
família — diferentes papéis, estados e valores — para que o comportamento
completo de cada controlo seja visível e interativo num único lugar.

Mude de separador clicando na barra de separadores ou com as teclas `Left`,
`Right`, `Home` e `End` e `Enter`. Clique num controlo para interagir com ele:
um interruptor comuta, um cursor desliza, um campo de texto recebe o cursor de
inserção, uma caixa de combinação abre. Um controlo clicado mantém o foco do
teclado, pelo que as setas, `Enter`, `Space` e os caracteres digitados o
comandam; `Tab` e `Shift+Tab` movem o foco entre a barra de separadores e os
controlos.

A galeria é iniciada a partir do menu iniciar do ambiente de trabalho ou pelo
nome a partir de uma shell. Requer uma sessão gráfica em curso: sem ela o canal
de janela é inacessível e a galeria comunica a recusa no fluxo de erro padrão e
termina.

## EXIT STATUS

Zero após um fecho limpo; diferente de zero quando o canal de janela ou a
região de tramas partilhada foi recusada (o motivo é indicado no fluxo de erro
padrão).
