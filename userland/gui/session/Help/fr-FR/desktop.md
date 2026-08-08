## NAME

desktop — démarrer la session graphique de bureau

## SYNOPSIS

`desktop`

## DESCRIPTION

Démarre la session graphique de bureau sur le poste de cette machine :
la commande acquiert le bail exclusif d'affichage et d'entrée du poste,
se connecte au service d'affichage et exécute le bureau composité — le
gestionnaire de fenêtres et la barre des tâches — jusqu'à la fin de la
session. La commande rend la main quand la session de bureau se
termine.

Le même bureau démarre automatiquement après l'authentification : une
connexion graphique (`os.loginType`) est la valeur par défaut sur une
machine capable d'en exécuter une. Cette commande le démarre à la
demande depuis un shell texte.

Quand aucun service d'affichage ne tourne, ou qu'une autre session
détient déjà le poste, la commande échoue en écrivant sa raison sur la
sortie d'erreur — elle ne déloge jamais une session en cours.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `desktop` — démarrer la session de bureau.

## EXIT STATUS

- `0` — l'aide courte a été servie.
- `2` — la ligne de commande n'a pas été comprise.
- tout autre code non nul — la session n'a pas pu démarrer (pas de
  poste, pas de service d'affichage) ou s'est terminée (le bail du
  poste a été perdu) ; la raison est écrite sur la sortie d'erreur.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `configure`
- `man`
