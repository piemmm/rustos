## NAME

unlink — remover um só nome

## SYNOPSIS

`unlink [--] ficheiro`

## DESCRIPTION

Remove exactamente um nome, através da única chamada ao sistema de
ficheiros que a função POSIX `unlink` nomeia. Não há deliberadamente
recursão, forçagem, confirmação nem relatórios: um guião que tenha de
remover um só nome e mais nada dispõe de uma ferramenta incapaz de fazer
mais. Para essas opções há o `rm`, e para um directório o `rmdir`.

O nome é removido **tal como é escrito**. Uma ligação simbólica é
removida ela mesma e nunca é seguida, pelo que uma ligação aí colocada
não pode desviar a remoção para o seu destino.

Um **directório** é recusado pelo sistema de ficheiros, no mesmo
percurso trancado que teria removido a entrada — não existe aqui
qualquer corrida entre verificar e remover.

É exigido exactamente um operando: nenhum operando e dois ou mais
operandos são ambos erros de utilização, e nada é removido. `--` termina
a análise de opções, pelo que um nome que começa por hífen continua
removível.

## OPTIONS

- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `unlink antigo.log` — remover um nome.
- `unlink Home:/Documents/alias` — remover a própria ligação simbólica,
  não aquilo que aponta.
- `unlink -- -nome-estranho` — remover um nome que começa por hífen.

## EXIT STATUS

- `0` — o nome foi removido (ou a ajuda breve foi escrita).
- `1` — o sistema de ficheiros recusou a remoção, ou a saída falhou; a
  razão é impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda breve (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

rm, rmdir, ln, link, readlink
