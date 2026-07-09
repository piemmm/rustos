## NAME

fstree — le gestionnaire de fichiers arborescent plein écran

## SYNOPSIS

`fstree [répertoire]`

## DESCRIPTION

Parcourt le système de fichiers dans une session plein écran pilotée au
clavier : un panneau d'arborescence de répertoires à gauche et un panneau
de fichiers à droite listant les entrées du répertoire sélectionné avec
leurs tailles et leurs dates de modification. La session démarre dans
`répertoire` (la vue racine `/` par défaut).

L'arborescence est lue paresseusement : le contenu d'un répertoire n'est
récupéré que lorsqu'il est affiché ou déplié pour la première fois, si
bien que parcourir un très grand volume ne coûte que les répertoires
réellement ouverts. Un répertoire que l'appelant ne peut pas lister est
refusé sur place — l'erreur apparaît sur la ligne de message et la vue
précédente est conservée ; rien n'est fabriqué.

Touches :

- `Haut`/`Bas` ou `k`/`j` — déplacer le curseur du panneau actif. Déplacer
  le curseur de l'arborescence liste le répertoire nouvellement
  sélectionné dans le panneau de fichiers.
- `Gauche`/`Droite` ou `h`/`l` — replier/déplier la ligne d'arborescence
  sous le curseur.
- `Entrée` — dans l'arborescence, bascule le dépliage ; dans le panneau de
  fichiers, descend dans le répertoire sélectionné (les deux panneaux
  suivent).
- `Tab` — changer de panneau actif.
- `s` — ouvrir le menu de tri : `n` nom, `e` extension, `s` taille,
  `m` date de modification, `r` inverser le sens, `Échap` annule. Les
  répertoires sont toujours groupés avant les fichiers.
- `c` — copier l'entrée sélectionnée : une invite demande la
  destination. Une destination relative atterrit dans le répertoire
  listé ; une destination qui est un répertoire existant reçoit la copie
  à l'intérieur, sous le nom de la source. Un répertoire est copié avec
  tout ce qu'il contient. Copier une entrée sur elle-même ou un
  répertoire dans son propre sous-arbre est refusé avant toute
  écriture.
- `m` — déplacer l'entrée sélectionnée, avec la même invite de
  destination. Au sein d'un même volume, le déplacement est un renommage
  atomique ; entre volumes, l'entrée est copiée puis la source
  supprimée.
- `r` — renommer l'entrée sélectionnée sur place : l'invite est
  pré-remplie avec le nom actuel.
- `d` — supprimer l'entrée sélectionnée après confirmation ; seul `y`
  procède. Supprimer un répertoire retire tout ce qu'il contient, et la
  confirmation le dit.
- `M` — créer un répertoire dans le répertoire listé ; son nom est
  demandé.
- `a` — modifier les bits de permission de l'entrée sélectionnée : une
  invite octale pré-remplie avec le mode actuel. Entrée applique (seul le
  propriétaire peut modifier — le noyau refuse quiconque d'autre), Échap
  annule.
- `.` — afficher/masquer les entrées cachées (noms à point) dans les deux
  panneaux.
- `?` — afficher cette aide par-dessus les panneaux ; toute touche la
  ferme.
- `q` — quitter en restaurant le terminal.

Quand une copie ou un déplacement écraserait un fichier existant, la
session demande fichier par fichier : `o` écrase, `s` saute (une source
sautée reste en place), et `c` annule les étapes restantes — ce qui a
déjà été appliqué le reste, et le rapport final dit ce qui s'est
passé. Un échec en cours de copie supprime la cible à moitié écrite et
affiche l'erreur du noyau ; rien ne se fait jamais passer pour une
copie complète. Chaque opération est autorisée par le noyau — un refus
apparaît tel quel sur la ligne de message, sans que rien ne change.

La ligne d'état montre le chemin listé, le nombre d'entrées visibles,
l'ordre de tri, les octets libres/totaux du volume sous-jacent (quand le
service d'information système peut les rapporter) et si les entrées
cachées sont affichées. Un fichier dont le format de stockage ne conserve
pas de date de modification affiche `-` dans la colonne de date.

Le marquage, la recherche et les visionneuses
texte/hexadécimale/désassemblage arrivent dans les étapes ultérieures
du plan de l'outil.

## OPTIONS

- `directory` — le répertoire de départ de la session ; la valeur par
  défaut est la vue racine `/`.
- `-h`, `-?` — afficher la forme courte de ce document et quitter.

## EXIT STATUS

- `0` — la session s'est terminée par le `q` de l'utilisateur.
- `1` — le répertoire de départ n'a pas pu être listé, ou le chemin du
  terminal a échoué.
- `2` — les arguments n'ont pas pu être compris.

## SEE ALSO

ls, cp, mv, rm, mkdir, chmod, du, df
