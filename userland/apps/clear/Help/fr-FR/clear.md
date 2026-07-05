## NAME

clear — effacer l'écran du terminal

## SYNOPSIS

`clear [-x]`

## DESCRIPTION

Écrit la séquence qui place le curseur dans le coin supérieur gauche
et efface tout l'affichage, laissant un écran vide. La séquence émise
dépend du terminal nommé par `TERM` ; un terminal incapable d'effacer
(un `TERM` inconnu se replie sur le profil minimal) fait échouer la
commande plutôt que d'imprimer des octets que le terminal afficherait
comme des caractères parasites.

Les consoles RustOS ne conservent aucun historique de défilement : il
n'y a donc rien à effacer de ce côté. `-x` (l'option GNU qui préserve
l'historique) est acceptée pour la compatibilité des scripts et ne
change rien.

## OPTIONS

- `-x` — acceptée pour la compatibilité GNU ; une console RustOS ne
  conserve aucun historique, la sortie est donc identique avec ou sans.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `clear` — effacer l'écran.

## EXIT STATUS

- `0` — la séquence d'effacement a été écrite.
- `1` — le terminal ne peut pas effacer, ou la sortie n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `TERM` — le terminal dont la séquence d'effacement est écrite.
- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `reset`
- `man`
