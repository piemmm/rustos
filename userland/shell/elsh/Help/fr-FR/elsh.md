## NAME

elsh — le shell de commandes de TAIRiX

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Lance un shell de commandes interactif — une boucle
lecture-évaluation-affichage sur les flux standard hérités. Un mot de
commande tapé est résolu d'abord parmi les commandes intégrées du
shell, puis dans le magasin d'applications système (`/System/Apps`),
puis dans les répertoires de la variable `PATH` ; le magasin est
cherché avant `PATH`, donc `PATH` ne peut jamais masquer une commande
système. Un mot non résolu sort avec `127` ; un paquet résolu mais non
exécutable sort avec `126`.

Les commandes intégrées :

- `cd <path>`, `pwd` — changer et afficher le répertoire de travail.
- `echo ...` — afficher ses opérandes.
- `export NAME=value`, `unset NAME` — modifier l'environnement exporté.
- `jobs`, `fg`, `bg` — contrôle des tâches.
- `ulimit` — lire et imposer des limites de ressources.
- `elevate` — exécuter une commande ré-authentifiée via le superviseur
  de connexion de la console.
- `help` — lister les commandes intégrées.
- `exit [code]` — terminer la session.

Le shell ne prend aucun opérande : l'exécution de scripts ne fait pas
encore partie de sa grammaire.

Sur un terminal, le shell offre un éditeur de ligne interactif :
Haut/Bas parcourent l'historique des commandes, `Ctrl-R` le recherche,
`Ctrl-C` abandonne la ligne en cours, `Ctrl-D` sur une ligne vide
termine la session, et Tab complète les noms de commandes, les chemins
et les références de ressources comme `sys:random`.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande et quitter.

## EXIT STATUS

- Le code de la commande intégrée `exit`, ou `0` quand le flux d'entrée
  se termine (ou que l'aide courte a été affichée).
- `2` — l'invocation n'a pas été comprise.

## ENVIRONMENT

- `PATH` — les répertoires cherchés après le magasin d'applications
  système.
- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`), exportée vers chaque commande lancée.

## SEE ALSO

- `man`
