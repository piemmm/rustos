## NAME

datetime — definir a data e a hora da máquina

## SYNOPSIS

`datetime`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que mostra o relógio da máquina
em seis campos editáveis — ano, mês e dia na primeira linha, hora, minuto
e segundo na segunda — e define o relógio para o que eles indicam. Nada
muda até que **Set** seja premido.

A leitura é UTC. O TAIRiX não mantém qualquer desvio de fuso horário,
pelo que não há hora local para mostrar nem para introduzir.

Normalmente chega-se à janela pelo menu do próprio relógio do ambiente de
trabalho: clicar no relógio na barra de ícones e escolher **Set Date &
Time…**. Definir o relógio exige uma autoridade que uma sessão de
ambiente de trabalho não tem, pelo que o ambiente de trabalho pede uma
conta que a tenha, e esta aplicação é iniciada como essa conta depois de
a palavra-passe ser aceite.

Clicar num campo para escrever nele, ou premir `Tab` para passar ao
seguinte. Apenas dígitos são aceites, com um `-` inicial permitido no ano
para uma data anterior ao ano 1. `Enter` define o relógio; `Escape` fecha
a janela.

Todos os campos são verificados antes de algo ser definido, e a primeira
falha é indicada na janela em vez de corrigida em silêncio: um mês fora
de 1 a 12, uma hora fora de 0 a 23, um minuto ou segundo fora de 0 a 59,
ou um dia que não existe no mês e ano introduzidos — 31 de abril, ou 29
de fevereiro fora de um ano bissexto. Nada é definido quando um campo é
recusado.

Datas anteriores a 1970 e muito posteriores a 2038 são entradas comuns. O
relógio é um valor de 64 bits com sinal, pelo que nenhuma delas é um
limite.

Se o relógio da máquina nunca foi definido desde que arrancou, os campos
abrem **vazios** e a janela di-lo. Não são preenchidos com a época Unix,
que seria uma data que a máquina nunca afirmou.

Se a conta com que esta aplicação está a correr não pode definir o
relógio, a tentativa é recusada, a janela di-lo, e o relógio fica
exatamente como estava. A razão é também escrita no fluxo de erro padrão.
A aplicação continua a correr: uma definição recusada é uma resposta, não
uma falha do programa.

## EXIT STATUS

Zero após um fecho limpo, incluindo quando uma definição foi recusada.
Diferente de zero quando a janela não pôde ser aberta, a região de frame
partilhada foi recusada, ou o canal da janela foi perdido; a razão é
indicada no fluxo de erro padrão.

## SEE ALSO

`sysinfo`, `uptime`
