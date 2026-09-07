## NAME

flock — executar um comando mantendo um bloqueio de ficheiro consultivo

## SYNOPSIS

`flock [options] ficheiro comando [argumento...]`

## DESCRIPTION

Toma um bloqueio consultivo que cobre todo o `ficheiro`, executa o
`comando` enquanto o mantém e termina com o estado do próprio comando. Duas
execuções que nomeiem o mesmo `ficheiro` nunca se sobrepõem, que é o que um
script precisa para ter apenas uma cópia de si mesmo em curso.

O bloqueio pertence ao ficheiro aberto por esta execução, pelo que o
sistema o liberta quando a execução termina: normalmente, interrompida ou
com falha. Nada é escrito no `ficheiro` e não há nada a limpar depois,
pelo que não fica um bloqueio obsoleto que a execução seguinte tenha de
esperar. O `ficheiro` é criado se não existir.

Os bloqueios são **consultivos**: coordenam programas que aceitam usá-los.
Não concedem nem retiram qualquer acesso, pelo que um programa que não tome
um bloqueio não fica impedido de ler ou escrever. Quem pode ler ou escrever
é decidido pelo proprietário, pelas permissões e pela lista de acesso do
ficheiro, tal como em qualquer outro.

Sem `-n` nem `-w` a execução espera o tempo que for preciso. Com `-n`
desiste de imediato; com `-w` após o tempo indicado. Em ambos os casos
desistir termina com o código de conflito (`1` salvo indicação de `-E`) e o
comando **não** é executado, pelo que um script distingue sempre «não
obtive o bloqueio» de «o comando falhou».

Três opções dos `flock` de outros sistemas faltam deliberadamente em vez
de serem aceites e ignoradas. `-u` e `-o` agem sobre um descritor de
ficheiro que uma consola abriu previamente, forma que este comando não
oferece; `-c` passa o seu argumento a uma consola, o que aqui se escreve
explicitamente `flock ficheiro elsh -c '...'` para que seja claro qual
executa. Pedir qualquer uma delas é um erro de utilização: o script é
avisado em vez de correr sem o bloqueio que pediu.

## OPTIONS

- `-s, --shared` — tomar um bloqueio partilhado. Várias execuções podem mantê-lo ao mesmo tempo, e todas excluem quem peça um exclusivo. É o bloqueio dos leitores.
- `-x, --exclusive` — tomar um bloqueio exclusivo, que exclui qualquer outro detentor. O valor por omissão e o bloqueio dos escritores.
- `-n, --nonblock` — não esperar: se o bloqueio estiver tomado, sair de imediato com o código de conflito.
- `-w, --timeout <seconds>` — esperar no máximo esse número de segundos inteiros e depois sair com o código de conflito.
- `-E, --conflict-code <n>` — o estado de saída quando `-n` ou `-w` desiste. Por omissão `1`. Escolha um valor que o comando nunca devolva se um script tiver de os distinguir.
- `-v, --verbose` — indicar na saída de erro se o bloqueio foi tomado.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `flock /Users/ian/Library/backup.lock backup-now` — iniciar a cópia de
  segurança, esperando se já houver outra em curso.
- `flock -n /Storage/db/data.lock compact` — compactar a base de dados, ou
  sair com `1` de imediato se o bloqueio estiver tomado.
- `flock -s -w 30 /Storage/db/data.lock report` — tomar um bloqueio de
  leitor, esperando até trinta segundos que um escritor termine.
- `flock -E 99 -n lock task` — sair com `99` em vez de `1` quando o
  bloqueio estiver tomado, para o distinguir de uma falha de `task`.

## EXIT STATUS

- o estado do próprio comando — o bloqueio foi tomado e o comando
  executou.
- o código de conflito (`1` por omissão) — `-n` ou `-w` desistiu; o comando
  não executou.
- `1` — o bloqueio não pôde ser tomado por uma razão que esperar não
  resolveria, ou o comando não pôde ser executado; a razão é impressa na
  saída de erro.
- `2` — a linha de comandos não foi compreendida; nada foi bloqueado nem
  executado.
- `126` — o comando foi encontrado mas não pôde ser executado.
- `127` — o comando não foi encontrado.

## ENVIRONMENT

- `PATH` — percorrido à procura do comando, depois dos armazéns de
  programas do sistema e do utilizador.
- `HOME` — localiza os armazéns de programas próprios do utilizador.
- `LANG` — a localização preferida para a ajuda breve (uma etiqueta BCP-47
  como `fr-FR`).

## SEE ALSO

elsh, ps, ulimit
