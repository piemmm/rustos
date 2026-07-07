## NAME

elsh — a shell de comandos do RustOS

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Executa uma shell de comandos interativa — um ciclo ler-avaliar-imprimir
sobre os fluxos padrão herdados. Uma palavra de comando escrita é
resolvida primeiro contra os builtins da shell, depois contra a loja de
aplicações do sistema (`/System/Apps`) e depois contra os diretórios da
variável `PATH`; a loja é procurada antes de `PATH`, pelo que `PATH`
nunca pode sombrear um comando do sistema. Uma palavra não resolvida
sai com `127`; um pacote resolvido mas não executável sai com `126`.

Os builtins:

- `cd <path>`, `pwd` — mudar e imprimir o diretório de trabalho.
- `echo ...` — imprimir os seus operandos.
- `export NAME=value`, `unset NAME` — editar o ambiente exportado.
- `jobs`, `fg`, `bg` — controlo de tarefas.
- `ulimit` — ler e impor limites de recursos.
- `elevate` — executar um comando reautenticado através do supervisor
  de início de sessão da consola.
- `help` — listar os builtins.
- `exit [code]` — terminar a sessão.

A shell não aceita operandos: a execução de scripts ainda não faz parte
da sua gramática.

Num terminal, a shell oferece um editor de linha interativo: Cima/Baixo
percorrem o histórico de comandos, `Ctrl-R` pesquisa-o, `Ctrl-C`
descarta a linha em edição, `Ctrl-D` numa linha vazia termina a sessão,
e Tab completa nomes de comandos, caminhos e referências de recursos
como `sys:random`.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste próprio comando e sair.

## EXIT STATUS

- O código do builtin `exit`, ou `0` quando o fluxo de entrada termina
  (ou a ajuda curta foi mostrada).
- `2` — a invocação não foi compreendida.

## ENVIRONMENT

- `PATH` — os diretórios procurados depois da loja de aplicações do
  sistema.
- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`), exportada para cada comando lançado.

## SEE ALSO

- `man`
