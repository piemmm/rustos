## NAME

printf — formater et afficher des données

## SYNOPSIS

`printf format [argument...]`

## DESCRIPTION

Affiche les `argument`(s) sous le contrôle de `format`, comme la
fonction C `printf`. Le format contient trois sortes d'éléments : des
caractères ordinaires, copiés sur la sortie standard ; des séquences
d'échappement ; et des directives de conversion `%`, chacune
convertissant l'argument suivant.

Les échappements sont `\a` (alerte), `\b` (retour arrière), `\c`
(terminer immédiatement toute sortie), `\e` (échappement), `\f` (saut de
page), `\n` (saut de ligne), `\r` (retour chariot), `\t` (tabulation),
`\v` (tabulation verticale), `\\`, `\"`, `\NNN` (un à trois chiffres
octaux), `\xHH` (un ou deux chiffres hexadécimaux) et `\uHHHH` /
`\UHHHHHHHH` (points de code Unicode, quatre ou huit chiffres
hexadécimaux).

Les conversions sont `%d`/`%i` (décimal signé), `%u` (décimal non
signé), `%o`/`%x`/`%X` (octal et hexadécimal), `%e`/`%E`/`%f`/`%F`/
`%g`/`%G`/`%a`/`%A` (virgule flottante), `%c` (le premier caractère de
l'argument), `%s` (chaîne), `%b` (chaîne dont les échappements sont
interprétés, l'octal s'écrivant `\0NNN`), `%q` (chaîne protégée pour
être réutilisée par un shell) et `%%` (un `%` littéral). Une directive
accepte les drapeaux C `-`, `+`, espace, `#`, `0` et `'`, une largeur de
champ et une précision ; la largeur et la précision peuvent chacune être
`*`, lisant leur valeur dans l'argument suivant. `%b` et `%q`
n'acceptent ni drapeau, ni largeur, ni précision.

Le format est réutilisé autant que nécessaire jusqu'à épuisement des
arguments ; une conversion sans argument restant affiche zéro ou la
chaîne vide. Un argument numérique est lu comme un nombre C
(hexadécimal `0x`, octal à `0` initial, virgule flottante, `inf`,
`nan`) ; un `'` ou `"` en tête convertit le point de code du caractère
suivant. Un argument qui n'est pas un nombre, ne l'est que
partiellement ou dépasse les bornes est diagnostiqué sur la sortie
d'erreur et converti aussi loin que possible — l'exécution continue et
se termine avec le code `1`. Une conversion inconnue, un drapeau sur une
conversion qui ne l'accepte pas ou un échappement mal formé arrête
l'exécution avec un diagnostic.

Deux divergences délibérées vis-à-vis du `printf` GNU : la virgule
flottante est calculée en double précision IEEE 754 (GNU utilise le
`long double`), si bien qu'une valeur au-delà du double affiche `inf` ;
et un *premier* argument `-h` ou `-?` affiche cette aide courte —
écrivez un tel format `printf -- -h...`.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande (premier argument
  uniquement).
- `--` — terminer l'analyse des options ; l'argument suivant est le
  format.

## EXAMPLES

- `printf '%s\n' hello` — afficher `hello` puis un saut de ligne.
- `printf '%d\n' 0x10` — afficher `16`.
- `printf '%5.2f|\n' 3.14159` — afficher ` 3.14|`.
- `printf '%s=%q\n' greeting 'hi there'` — afficher
  `greeting='hi there'`.
- `printf '%b' 'one\ntwo\n'` — afficher deux lignes depuis un seul
  argument.
- `printf '%s-' a b c` — réutiliser le format : `a-b-c-`.

## EXIT STATUS

- `0` — tout (ou l'aide courte demandée) a été écrit.
- `1` — un problème de conversion a été diagnostiqué, le format était
  absent ou invalide, un échappement était mal formé, ou la sortie n'a
  plus accepté d'octets.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `seq`
- `man`
