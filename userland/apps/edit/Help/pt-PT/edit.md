## NAME

edit — editor de texto de ecrã inteiro

## SYNOPSIS

`edit [file] [-h | -?]`

## DESCRIPTION

Um editor de texto de ecrã inteiro no espírito do clássico editor do
QuickBasic / MS-DOS: uma barra de menus no topo, o texto por baixo e
uma linha de estado com o nome do ficheiro, a posição do cursor e as
dicas de teclas. Edita um ficheiro de cada vez.

Iniciado com um operando `file`, o editor carrega esse ficheiro; um
ficheiro que ainda não existe abre como um buffer vazio e é criado na
primeira gravação. Iniciado sem operando, abre um buffer sem nome e
pede um nome quando é gravado pela primeira vez.

O menu (aberto com `F10` ou com `Alt` mais a letra realçada de um
título — `Alt-F` para `File`, `Alt-S` para `Search` — navegado com as
setas, `Enter` seleciona, `Esc` ou `F10` fecha) contém:

- `File` — `New`, `Open...`, `Save`, `Save As...`, `Exit`.
- `Search` — `Find...`, `Repeat Last Find`.

Quando uma ação descartaria alterações não gravadas (`New`, `Open...`,
`Exit`), o editor pergunta primeiro: `y` grava e continua, `n`
descarta, `c` (ou `Esc`) cancela.

Teclas dentro da sessão:

- Escrever insere no cursor; `Insert` alterna a sobrescrita (`OVR` na
  linha de estado).
- `Enter` divide a linha; `Backspace` e `Delete` removem carateres e
  juntam linhas nos fins de linha.
- As setas, `Home`, `End`, `PageUp`, `PageDown` movem o cursor; a
  vista desloca-se, também horizontalmente, para o seguir.
- `Tab` insere espaços até à próxima paragem de oito colunas.
- `F1` mostra o resumo de teclas, `F2` grava, `F3` repete a última
  procura, `F10` (ou `Alt-F` / `Alt-S`) abre o menu.

`Find...` procura para a frente a partir do cursor, literalmente e
distinguindo maiúsculas, dando a volta no fim do buffer; uma procura
sem correspondência reporta `Match not found` e deixa o cursor onde
estava.

O editor edita apenas ficheiros de texto e diz exatamente o que muda:

- O ficheiro tem de ser texto UTF-8 com não mais de 16 MiB; qualquer
  outra coisa (um ficheiro binário, um retorno de carro isolado, um
  ficheiro demasiado grande) é recusada com a razão declarada — nunca
  aberta como lixo.
- Os carateres de tabulação são expandidos para espaços em paragens de
  oito colunas ao carregar, e os fins de linha CRLF tornam-se LF; cada
  conversão é anunciada na linha de estado, nunca aplicada em silêncio.
- A presença ou ausência da mudança de linha final do ficheiro é
  preservada.

Um carregamento ou gravação falhados dentro da sessão são reportados
na linha de estado e o buffer é mantido; a sessão nunca morre por um
ficheiro recusado. Cada caminho é resolvido e verificado quanto a
permissões pelo núcleo sob a identidade do próprio chamador — o editor
não detém autoridade especial.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste próprio comando e sair.

## EXIT STATUS

- `0` — a sessão terminou por `File > Exit`, ou a ajuda curta foi
  mostrada.
- `1` — o ficheiro nomeado não pôde ser carregado (não é texto,
  demasiado grande ou recusado), ou o terminal falhou; a razão é
  impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).
- `TERM` — o terminal para o qual a sessão desenha; um valor
  desconhecido ou ausente degrada para uma base segura.

## SEE ALSO

- `cat`
- `man`
