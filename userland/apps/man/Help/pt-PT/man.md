## NAME

man — mostrar o documento de ajuda de um comando

## SYNOPSIS

`man [-h | -?] <command> [topic]`

## DESCRIPTION

Apresenta o documento de ajuda que o pacote de aplicação de um comando
inclui, na sua língua quando existe uma tradução.

Cada programa RustOS é um pacote de aplicação com uma árvore `Help/`: um
documento estruturado por comando ou tópico, por língua. O `man` resolve
`<command>` exatamente como a shell — primeiro a loja de aplicações do
sistema, depois os diretórios de `PATH` — pelo que a página mostrada
documenta sempre o programa que a shell executaria para a mesma palavra.
Um sufixo `.app` nomeia o pacote diretamente. Quando nem a loja nem o
`PATH` contêm a palavra, o `man` percorre as lojas de aplicações de forma
recursiva — primeiro `/Apps`, depois a pasta `Apps` do seu diretório
pessoal — pelo que um pacote arrumado em pastas encaixadas é encontrado à
mesma; a procura nunca olha para dentro de outro pacote, e vence a
correspondência menos profunda.

O documento é escolhido segundo a região da variável de ambiente `LANG`,
recuando para a mesma língua noutra região e por fim para o documento
canónico em inglês. Quando a página não é mostrada na língua pedida, o
`man` assinala a substituição no fluxo consultivo (fd 3); a página em si
nunca mistura línguas.

Numa consola interativa a página é mostrada ecrã a ecrã: a barra de
espaços vira a página, enter avança uma linha e `q` termina. Quando a
saída está redirecionada ou o tamanho da consola é desconhecido, a página
inteira é emitida de seguida.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `man ps` — mostrar a página do `ps`.
- `man top keys` — mostrar o tópico `keys` do pacote `top`.
- `man files.app` — nomear o pacote diretamente.

## EXIT STATUS

- `0` — a página foi mostrada.
- `1` — o comando ou o seu documento de ajuda não foi encontrado, ou a
  página não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida (uma etiqueta BCP-47 como `pt-PT`).
- `PATH` — os diretórios adicionais onde procurar pacotes
  `<command>.app`, depois da loja de aplicações do sistema.
- `HOME` — nomeia a sua própria pasta `Apps` para a procura recursiva de
  pacotes.

## SEE ALSO

- `elsh`
