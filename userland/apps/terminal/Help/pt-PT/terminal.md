## NAME

terminal — emulador de terminal gráfico

## SYNOPSIS

`terminal`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que aloja a shell predefinida
do utilizador num ecrã de 80×24 caracteres. As teclas escritas na
janela focada são enviadas à shell; tudo o que a shell escreve (tanto
a saída padrão como o erro padrão) é interpretado através do
vocabulário ANSI/VT partilhado e desenhado com a paleta do tema ativo.
O terminal em si nunca faz eco: o eco e a edição de linha pertencem à
shell, exatamente como numa consola.

O terminal é lançado a partir do menu iniciar do ambiente de trabalho
(a entrada `Terminal`) ou pelo nome a partir de uma shell. Requer uma
sessão gráfica em execução: sem ela, o canal de janela é inalcançável
e o terminal comunica a recusa no fluxo de erro padrão e termina.

A sessão termina quando a shell sai (por exemplo com `exit`) ou quando
a janela é fechada a partir do ambiente de trabalho; fechar a janela
termina a shell com fim de ficheiro na sua entrada.

## EXIT STATUS

Zero após um fecho limpo ou a saída da própria shell; diferente de
zero quando a shell não pôde ser alojada ou quando o canal de janela,
a região de quadros partilhada ou a caixa de eventos foi recusada (a
razão é indicada no fluxo de erro padrão).
