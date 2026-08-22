## NAME

terminal — emulador de terminal gráfico

## SYNOPSIS

`terminal`

## DESCRIPTION

Abre uma janela do ambiente de trabalho que aloja a shell predefinida
do utilizador num ecrã de 80×25 caracteres. As teclas escritas na
janela focada são enviadas à shell; tudo o que a shell escreve (tanto
a saída padrão como o erro padrão) é interpretado através do
vocabulário ANSI/VT partilhado e desenhado com o esquema de cores
escolhido nas definições. O terminal em si nunca faz eco: o eco e a
edição de linha pertencem à shell, exatamente como numa consola.

A janela abre-se com as medidas que o ecrã de 80×25 mede no tamanho de
texto em vigor, para que se ajuste ao ecrã onde é apresentada; num ecrã
demasiado pequeno para esse tamanho, o texto é reduzido em vez de a
janela ser estreitada, porque um programa que se desenha para 80 colunas
deve continuar a obtê-las.

O terminal é lançado a partir da Biblioteca de programas do ambiente de
trabalho (o botão `Library` da barra de tarefas) ou pelo nome a partir
de uma shell. Requer uma sessão gráfica em execução: sem ela, o canal de
janela é inalcançável e o terminal comunica a recusa no fluxo de erro
padrão e termina.

A sessão termina quando a shell sai (por exemplo com `exit`) ou quando
a janela é fechada a partir do ambiente de trabalho; fechar a janela
termina a shell com fim de ficheiro na sua entrada.

Ao premir o botão secundário (direito) do rato em qualquer lugar do ecrã
abre-se o menu do terminal. Cada linha tem um atalho de teclado que
funciona quer o menu esteja aberto ou não, e `Escape` — ou um clique
fora do menu — descarta-o sem escolher nada.

| Linha | Atalho | O que faz |
| --- | --- | --- |
| Definições… | `Ctrl ,` | Abre as definições descritas abaixo. |
| Texto maior | `Ctrl +` | Desenha o ecrã um passo maior. |
| Texto menor | `Ctrl -` | Desenha o ecrã um passo menor. |
| Tamanho real | `Ctrl 0` | Regressa ao tamanho de texto predefinido. |
| Limpar ecrã | `Ctrl Shift K` | Esvazia o ecrã sem escrever na shell. |
| Fechar | `Ctrl Shift W` | Fecha a janela e termina a shell. |

As definições abrem-se na própria janela e têm dois separadores.
**Aparência** escolhe o esquema de cores, define o tamanho do texto e
edita o esquema próprio do utilizador. Os esquemas incluídos são
*System* (que segue a aparência escura ou clara do ambiente de
trabalho), *Midnight*, *Phosphor*, *Amber*, *Ember*, *Contrast*,
*Paper* e *Custom*. Escolher *Custom* utiliza as cores editadas por
baixo do seletor: uma grelha das vinte cores de que um ecrã é composto —
o fundo, o primeiro plano, o cursor, o texto do cursor e as dezasseis
cores ANSI — com seletores de vermelho, verde e azul para a que estiver
selecionada.

**Efeitos** define como o ecrã é desenhado.

| Efeito | O que faz |
| --- | --- |
| Opacidade | O quão sólido é o fundo. Abaixo do total, o ambiente de trabalho transparece por trás do texto, que permanece totalmente legível. |
| Desfocagem de fundo | O quanto o ambiente de trabalho por trás de uma janela transparente é desfocado. Não tem efeito numa janela totalmente opaca. |
| Linhas de varrimento | Atenua as linhas alternadas, a parte plana do aspeto de uma máscara de sombra. |
| Ruído | Um ruído de fundo por píxel em movimento, como o que tem um sinal analógico. |
| Fósforo | Quanto tempo persistem os píxeis acesos, de modo que o texto que desliza rápido deixa um rasto. |
| Oscilação | Uma lenta ondulação horizontal móvel, como a de um tubo fora de tempo. |

Cada alteração surte efeito imediatamente e é guardada no perfil próprio
do utilizador, de modo que um terminal posterior se abra da mesma forma.
O sistema operativo guarda o perfil através do seu serviço de
configurações, e ele é privado do terminal: nenhuma outra aplicação o
pode ler ou alterar. Apenas o que o utilizador realmente mudou é
armazenado, pelo que *Restaurar predefinições* remove essas escolhas em
vez de congelar os valores de hoje — aplica-se então o que o
administrador ou uma versão posterior do terminal alterar. Uma
configuração que o terminal não consegue interpretar fica na sua
predefinição e é comunicada no fluxo de erro padrão, e um serviço de
configurações inacessível deixa o terminal a funcionar com os valores com
que é distribuído, o que também é comunicado.

## EXIT STATUS

Zero após um fecho limpo ou a saída da própria shell; diferente de
zero quando a shell não pôde ser alojada ou quando o canal de janela,
a região de quadros partilhada ou a caixa de eventos foi recusada (a
razão é indicada no fluxo de erro padrão).

## ENVIRONMENT

`HOME`
: A pasta pessoal da conta, onde o terminal lê e escreve o seu perfil.
Sem ela, o terminal funciona com o perfil predefinido e não guarda nada.

`TERM`
: Exportada para a shell alojada como `xterm-256color`, nomeando o
emulador que este terminal apresenta. Qualquer valor herdado é
substituído; o resto do ambiente é encaminhado para a shell sem
alterações.

## SEE ALSO

`elsh`, `sysinfo`
