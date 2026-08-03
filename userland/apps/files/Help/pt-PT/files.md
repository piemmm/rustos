## NAME

files — navegador gráfico do sistema de ficheiros

## SYNOPSIS

`files [directory] [-h | -?]`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que lista o sistema de
ficheiros, começando pelo `directory` nomeado na linha de comandos, ou
pelo diretório pessoal do utilizador que a lançou quando nenhum é
nomeado. A linha superior mostra o caminho do diretório atual; as
linhas seguintes listam as entradas do diretório, com a entrada
selecionada realçada com a cor de destaque do tema ativo. Cada leitura
de diretório é uma listagem normal, com verificação de permissões, sob
a identidade do utilizador que a lançou: um diretório ilegível é
recusado, nunca adivinhado.

O navegador é lançado a partir do botão permanente `Files` da barra de
tarefas ou pelo nome a partir de uma shell. Requer uma sessão gráfica
em execução: sem ela, o canal de janela fica inalcançável e o navegador
comunica a recusa no fluxo de erro padrão e termina.

A janela controla-se com o teclado: `Baixo` e `Cima` movem a seleção,
`Enter` abre o diretório selecionado e `Backspace` sobe para o
diretório pai. Fechar a janela a partir do ambiente de trabalho termina
o navegador.

O operando `directory` é tratado como entrada não fidedigna: tem de ser
um caminho absoluto dentro do limite de comprimento de caminho do
sistema, e cada um dos seus componentes tem de ser um nome de diretório
verdadeiro — `.` e `..` não o são, pelo que uma escrita nunca pode
significar outro lugar que não aquele que se lê. Um diretório que
infrinja alguma dessas regras, ou que o utilizador que a lançou não
possa listar, é recusado com a razão no fluxo de erro padrão e a janela
abre-se antes no diretório pessoal, para que um argumento errado nunca
deixe o utilizador sem janela. Um segundo operando é recusado de todo
em vez de ignorado.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste próprio comando e sair.

## EXIT STATUS

Zero após um fecho limpo, ou depois de mostrada a ajuda curta; `2`
quando a linha de comandos não foi compreendida; caso contrário,
diferente de zero quando o canal de janela, a região de fotogramas
partilhada ou a listagem inicial do diretório foi recusada (o motivo é
indicado no fluxo de erro padrão).
