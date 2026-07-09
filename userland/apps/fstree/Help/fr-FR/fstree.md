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
- `.` — afficher/masquer les entrées cachées (noms à point) dans les deux
  panneaux.
- `?` — afficher cette aide par-dessus les panneaux ; toute touche la
  ferme.
- `q` — quitter en restaurant le terminal.

La ligne d'état montre le chemin listé, le nombre d'entrées visibles,
l'ordre de tri, les octets libres/totaux du volume sous-jacent (quand le
service d'information système peut les rapporter) et si les entrées
cachées sont affichées. Un fichier dont le format de stockage ne conserve
pas de date de modification affiche `-` dans la colonne de date.

Les opérations sur fichiers (copier, déplacer, renommer, supprimer), le
marquage, la recherche et les visionneuses texte/hexadécimale/désassemblage
arrivent dans les étapes ultérieures du plan de l'outil.

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

ls, du, df
