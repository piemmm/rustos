## NAME

configure — lire et régler la configuration système au démarrage

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Liste, affiche et règle les paramètres du magasin de configuration
situé à `/System/Settings/Configuration/system.conf`. Sans opérande,
chaque paramètre est listé avec sa valeur actuelle ; avec une clé
seule, la valeur de ce paramètre est affichée ; avec une clé et une
valeur, le paramètre est modifié.

Le magasin réside sur le volume racine chiffré et n'est lu par ses
consommateurs qu'après le déverrouillage du système de fichiers
racine ; une modification prend donc effet au prochain démarrage de son
consommateur (`os.loginType` : la connexion du prochain démarrage).

L'ensemble des clés est fermé : une clé inconnue, ou une valeur hors de
l'ensemble d'une clé, est refusée avec l'énoncé des choix valides et ne
change rien. Modifier un paramètre réécrit le magasin sous sa forme
canonique et exige le droit d'écriture sur `/System/Settings` — un
compte ordinaire peut lire les paramètres mais pas les changer.

- `os.loginType` — `text` ou `graphical` : le type de session que le
  service de connexion lance pour un utilisateur authentifié. `text`
  (la valeur par défaut) lance le shell du compte — le bureau peut
  toujours être lancé à la demande avec la commande `desktop` ;
  `graphical` lance directement la session de bureau après
  l'authentification quand un bureau est installé, et se replie sur le
  texte sinon.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `configure` — lister tous les paramètres.
- `configure os.loginType` — afficher le type de session par défaut.
- `configure os.loginType graphical` — démarrer sur la connexion
  graphique.

## EXIT STATUS

- `0` — la liste, la valeur, l'aide courte ou la modification a été
  effectuée.
- `1` — le magasin n'a pas pu être lu ou écrit (par exemple l'appelant
  ne peut pas modifier les réglages système), ou la sortie n'a pas pu
  être délivrée.
- `2` — la ligne de commande n'a pas été comprise, la clé est inconnue
  ou la valeur est hors de l'ensemble de la clé.

## ENVIRONMENT

- `LANG` — la langue préférée de l'aide courte (une étiquette BCP-47
  comme `fr-FR`).

## SEE ALSO

- `man`
