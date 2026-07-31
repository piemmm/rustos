## NAME

files — navegador gráfico do sistema de ficheiros

## SYNOPSIS

`files`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que lista o sistema de
ficheiros, começando pela vista de raiz. A linha superior mostra o
caminho do diretório atual; as linhas seguintes listam as entradas do
diretório, com a entrada selecionada realçada com a cor de destaque do
tema ativo. Cada leitura de diretório é uma listagem normal, com
verificação de permissões, sob a identidade do utilizador que a
lançou: um diretório ilegível é recusado, nunca adivinhado.

O navegador é lançado a partir do botão permanente `Files` da barra de
tarefas ou pelo nome a partir de uma shell. Requer uma sessão gráfica
em execução: sem ela, o canal de janela fica inalcançável e o navegador
comunica a recusa no fluxo de erro padrão e termina.

A janela controla-se com o teclado: `Baixo` e `Cima` movem a seleção,
`Enter` abre o diretório selecionado e `Backspace` sobe para o
diretório pai. Fechar a janela a partir do ambiente de trabalho termina
o navegador.

## EXIT STATUS

Zero após um fecho limpo; diferente de zero quando o canal de janela, a
região de fotogramas partilhada ou a listagem inicial do diretório foi
recusada (o motivo é indicado no fluxo de erro padrão).
