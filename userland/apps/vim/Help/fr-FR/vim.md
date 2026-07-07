## NAME

vim — l'éditeur de texte modal

## SYNOPSIS

`vim [-R] [+num | + | +/pattern] [--] [file ...]`

## DESCRIPTION

Édite des fichiers texte avec le jeu de commandes modal du célèbre
éditeur vim. La session démarre en mode normal : les touches sont des
commandes, et `i` (ou `a`, `o` et leurs variantes) entre en mode
insertion, où la frappe devient du texte. `Esc` revient au mode
normal. `:q` quitte ; `:wq` (ou `ZZ`) écrit puis quitte.

Plusieurs fichiers peuvent être nommés ; la session ouvre le premier
et `:n` / `:prev` parcourent la liste d'arguments. Un fichier encore
inexistant est un `[New File]`, créé à la première écriture.

Commandes du mode normal (le cœur de vim mis en œuvre) :

- Déplacements : `h j k l`, les flèches, `w W b B e E`, `0 ^ $`,
  `f F t T` avec répétition `;`/`,`, `gg G`, `{ }`, `%`, `H M L` et
  `Enter`. Un préfixe numérique répète un déplacement : `3w`.
- Opérateurs : `d` (supprimer), `c` (changer), `y` (copier), appliqués
  sur tout déplacement ou objet textuel (`iw aw i( a( i[ i{ i" i' i<`
  et leurs paires) ; doublés (`dd cc yy`), ils agissent sur des lignes
  entières. Raccourcis : `x X s S D C Y r ~ J`.
- Registres : `"a`–`"z` avant un opérateur ou un collage choisit un
  registre nommé ; les majuscules ajoutent à la suite. `p`/`P` colle
  après/avant le curseur.
- Historique : `u` annule des changements entiers, `Ctrl-R` les
  rétablit, et `.` répète le dernier changement (texte inséré
  compris).
- Recherche : `/pattern` en avant, `?pattern` en arrière, `n`/`N`
  répètent, `*` cherche le mot sous le curseur. Les motifs acceptent
  les littéraux, `.`, `*`, `^`, `$`, les classes `[...]` et les
  limites de mot `\<` `\>`. Les occurrences restent surlignées
  jusqu'à `:noh`.
- Sélection visuelle : `v` (caractères) et `V` (lignes), étendue par
  tout déplacement ou objet textuel, puis traitée avec `d x c s y J`.
- Défilement : `Ctrl-D Ctrl-U` (demi-fenêtre), `Ctrl-F Ctrl-B` et
  PgPréc/PgSuiv (fenêtre entière) ; `Ctrl-G` affiche le résumé du
  fichier.

Le cœur ex (`:`) : `:w [file]`, `:q`, `:wq`, `:x`, `:e file`,
`:enew`, `:r file`, `:n`, `:prev`, `:noh`, `:set number` /
`:set nonumber`, les adresses de ligne (`:12`, `:$`, `:.+2`),
`:[range]d` et `:[range]s/pattern/replacement/[g]` (avec `&` pour
l'occurrence entière dans le remplacement, `%` pour toutes les lignes
de la plage). Un `!` après `w`, `q` ou `e` force malgré la lecture
seule ou les changements non écrits.

Tout ce que vim offre au-delà de ce cœur est prévu pour des étapes
ultérieures ; la liste vit dans `plans/VIM.md` de l'arbre source.

## OPTIONS

- `-R` — lecture seule : le tampon s'édite en mémoire, mais `:w` est
  refusé sauf à forcer avec `:w!`.
- `+num` — commencer à la ligne `num` du premier fichier.
- `+` — commencer à la dernière ligne du premier fichier.
- `+/pattern` — commencer à la première occurrence de `pattern` dans
  le premier fichier.
- `--` — fin des options ; tout argument suivant est un nom de
  fichier.
- `-h, -?` — afficher l'aide brève propre à cette commande et quitter.

## EXIT STATUS

- `0` — la session s'est terminée par une commande de sortie, ou
  l'aide brève a été affichée.
- `1` — le terminal a échoué ; la raison est imprimée sur la sortie
  d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la langue préférée de l'aide brève (une étiquette BCP-47
  telle que `fr-FR`).
- `TERM` — le profil de terminal de la session ; les valeurs inconnues
  se replient sur la base simple.

## SEE ALSO

- `man`
- `cat`
