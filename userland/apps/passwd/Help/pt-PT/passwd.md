## NAME

passwd — definir a palavra-passe de uma conta

## SYNOPSIS

`passwd [--record RECORD] [--] NAME`

## DESCRIPTION

Substitui a palavra-passe guardada da conta nomeada. Definir uma
palavra-passe é uma operação administrativa: a base recusa quem não possua a
capacidade de administração de utilizadores.

Nenhuma palavra-passe em claro atravessa a chamada de sistema. A ferramenta
pergunta duas vezes com o eco do terminal desligado, reduz o que foi escrito
a um registo PBKDF2 com sal — tirado da fonte aleatória do núcleo — e envia
o registo; ambos os tampões em claro são postos a zero assim que ele existe.

O nome da conta é obrigatório. O `passwd` do GNU sem operando muda a
palavra-passe do próprio chamador, o que em TAIRiX exigiria uma via de
autoatendimento sem privilégios que não existe: toda a interface de
administração de contas é protegida por capacidade, e recortar nela «o teu
próprio registo» seria uma mudança do modelo de segurança, não uma
conveniência.

Um chamador sem terminal — um programa gráfico, cuja entrada padrão está
fechada sob o intermediário de elevação — calcula ele próprio o registo e
entrega o já feito com `--record`, pelo que não existe claro de nenhum dos
lados. O registo é verificado como bem formado antes de ser guardado.

`--` termina a análise de opções: todos os argumentos seguintes são operandos.

## OPTIONS

- `--record RECORD` — um registo PBKDF2 com sal já pronto, para um chamador
  sem terminal onde perguntar.
- `-h, -?, --help` — mostrar a ajuda curta do próprio comando.

## EXAMPLES

- `passwd ada` — perguntar duas vezes e definir a palavra-passe da conta.

## EXIT STATUS

- `0` — a palavra-passe foi substituída.
- `1` — a base recusou ou falhou a substituição, as duas entradas não
  coincidiram, não havia aleatoriedade disponível, ou o registo estava
  malformado; a razão é escrita no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a locale preferida para a ajuda curta (uma etiqueta BCP-47 como `pt-PT`).

## SEE ALSO

- `useradd`
- `usermod`
- `userdel`
- `users`
