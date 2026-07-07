## NAME

seq — afficher une suite de nombres

## SYNOPSIS

`seq [-f format] [-s string] [-w] [premier [pas]] dernier`

## DESCRIPTION

Affiche les nombres de `premier` à `dernier`, par pas de `pas`, un par
ligne par défaut. Un `premier` ou un `pas` omis vaut 1 — y compris quand
`dernier` est plus petit que `premier`, si bien que `seq 5 1` n'affiche
rien. La suite s'arrête lorsque l'ajout du `pas` dépasserait `dernier`.

Les trois opérandes sont lus comme des valeurs en virgule flottante ;
`pas` est en général positif quand `premier` est inférieur à `dernier`
et négatif dans le cas contraire, et ne peut pas être nul. `dernier`
peut être `inf` pour compter sans fin. La précision d'affichage par
défaut suit l'écriture des opérandes (`seq 1 0.25 2` affiche deux
décimales), et les suites de nombres entiers sont générées exactement,
quelle que soit leur taille.

L'analyse des options s'arrête au premier opérande, et un nombre négatif
en tête est un opérande, pas une option : `seq -5 5` compte depuis -5.

## OPTIONS

- `-f, --format <format>` — afficher chaque nombre via le `<format>` en
  virgule flottante de style printf (une seule directive `%` de type
  `e`, `f`, `g` ou `a`, en majuscule ou minuscule, avec les drapeaux, la
  largeur et la précision habituels). Incompatible avec `-w`.
- `-s, --separator <string>` — séparer les nombres par `<string>` au
  lieu d'un saut de ligne. La sortie se termine toujours par un saut de
  ligne.
- `-w, --equal-width` — compléter chaque nombre par des zéros de tête
  jusqu'à une largeur commune. Incompatible avec `-f`.
- `-h, -?` — afficher l'aide courte de cette commande.
- `--` — terminer l'analyse des options ; tout argument suivant est un
  opérande.

## EXAMPLES

- `seq 5` — afficher 1 à 5.
- `seq 2 5` — afficher 2 à 5.
- `seq 1 2 10` — afficher les nombres impairs de 1 à 9.
- `seq 5 -1 1` — compter à rebours de 5 à 1.
- `seq -w 8 10` — afficher `08`, `09`, `10`.
- `seq -s , 3` — afficher `1,2,3`.
- `seq -f %.2f 3` — afficher `1.00`, `2.00`, `3.00`.

## EXIT STATUS

- `0` — la suite (ou l'aide courte demandée) a été écrite.
- `1` — la sortie n'a plus accepté d'octets.
- `2` — la ligne de commande n'a pas été comprise (option inconnue,
  nombre invalide, pas nul ou format incorrect).

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `yes`
- `man`
