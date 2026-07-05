## NAME

reset — remettre le terminal dans un état sain

## SYNOPSIS

`reset`

## DESCRIPTION

Défait l'état qu'un programme plein écran planté peut laisser derrière
lui. D'abord la discipline d'entrée est ramenée au réglage interactif
par défaut (les caractères tapés s'affichent à nouveau). Ensuite la
séquence de restauration est écrite : quitter l'écran alternatif,
réafficher le curseur, réinitialiser couleurs et attributs,
réinitialiser la région de défilement, puis placer le curseur en haut à
gauche et effacer l'affichage.

Les opérations émises dépendent du terminal nommé par `TERM` ; une
opération que le terminal ne comprend pas est omise. Un terminal sans
aucun contrôle (un `TERM` inconnu se replie sur le profil minimal) ne
reçoit que la restauration de la discipline d'entrée.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `reset` — restaurer le terminal après le plantage d'un programme
  plein écran.

## EXIT STATUS

- `0` — le terminal a été restauré.
- `1` — la sortie n'a pas pu être délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `TERM` — le terminal dont la séquence de restauration est écrite.
- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `clear`
- `man`
