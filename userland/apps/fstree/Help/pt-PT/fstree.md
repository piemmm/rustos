## NAME

fstree — o gestor de ficheiros em árvore de ecrã inteiro

## SYNOPSIS

`fstree [diretório]`

## DESCRIPTION

Percorre o sistema de ficheiros numa sessão de ecrã inteiro guiada pelo
teclado: um painel com a árvore de diretórios à esquerda e um painel de
ficheiros à direita que lista as entradas do diretório selecionado com os
seus tamanhos e datas de modificação. A sessão começa em `diretório`
(a vista raiz `/` por omissão).

A árvore é lida preguiçosamente: o conteúdo de um diretório só é obtido
quando é mostrado ou expandido pela primeira vez, pelo que percorrer um
volume enorme custa apenas os diretórios realmente abertos. Um diretório
que o chamador não pode listar é recusado no local — o erro aparece na
linha de mensagens e a vista anterior mantém-se; nada é fabricado.

Teclas:

- `Cima`/`Baixo` ou `k`/`j` — mover o cursor do painel ativo. Mover o
  cursor da árvore lista o diretório recém-selecionado no painel de
  ficheiros.
- `Esquerda`/`Direita` ou `h`/`l` — recolher/expandir a linha da árvore
  sob o cursor.
- `Enter` — na árvore, alterna a expansão; no painel de ficheiros, desce
  ao diretório selecionado (ambos os painéis acompanham).
- `Tab` — trocar o painel ativo.
- `s` — abrir o menu de ordenação: `n` nome, `e` extensão, `s` tamanho,
  `m` data de modificação, `r` inverter o sentido, `Esc` cancela. Os
  diretórios agrupam-se sempre antes dos ficheiros.
- `a` — editar os bits de permissão da entrada selecionada: uma linha
  octal pré-preenchida com o modo atual. Enter aplica (só o proprietário
  pode alterá-lo — o núcleo recusa qualquer outro), Esc cancela.
- `.` — mostrar/ocultar as entradas ocultas (nomes com ponto) em ambos os
  painéis.
- `?` — mostrar esta ajuda sobre os painéis; qualquer tecla fecha-a.
- `q` — sair, restaurando o terminal.

A linha de estado mostra o caminho listado, o número de entradas
visíveis, a ordem de ordenação, os bytes livres/totais do volume
subjacente (quando o serviço de informação do sistema os pode reportar)
e se as entradas ocultas estão visíveis. Um ficheiro cujo formato de
armazenamento não guarda data de modificação mostra `-` na coluna da
data.

As operações sobre ficheiros (copiar, mover, renomear, apagar), a
marcação, a pesquisa e os visualizadores de texto/hexadecimal/
desassemblagem chegam em fases posteriores do plano da ferramenta.

## OPTIONS

- `directory` — o diretório onde a sessão começa; o valor por omissão é a
  vista raiz `/`.
- `-h`, `-?` — imprimir a forma curta deste documento e sair.

## EXIT STATUS

- `0` — a sessão terminou com o `q` do utilizador.
- `1` — o diretório inicial não pôde ser listado, ou o caminho do
  terminal falhou.
- `2` — os argumentos não puderam ser compreendidos.

## SEE ALSO

ls, du, df
