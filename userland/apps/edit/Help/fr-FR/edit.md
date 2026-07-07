## NAME

edit — éditeur de texte plein écran

## SYNOPSIS

`edit [fichier] [-h | -?]`

## DESCRIPTION

Un éditeur de texte plein écran dans l'esprit de l'éditeur classique
QuickBasic / MS-DOS : une barre de menus en haut, le texte en dessous,
et une ligne d'état affichant le nom du fichier, la position du
curseur et les touches principales. Il édite un fichier à la fois.

Lancé avec un opérande `fichier`, l'éditeur charge ce fichier ; un
fichier qui n'existe pas encore s'ouvre comme tampon vide et est créé
au premier enregistrement. Lancé sans opérande, il ouvre un tampon
sans nom et demande un nom au premier enregistrement.

Le menu (ouvert avec `F10`, parcouru avec les flèches, `Enter`
sélectionne, `F10` ferme) propose :

- `File` — `New`, `Open...`, `Save`, `Save As...`, `Exit`.
- `Search` — `Find...`, `Repeat Last Find`.

Quand une action abandonnerait des modifications non enregistrées
(`New`, `Open...`, `Exit`), l'éditeur demande d'abord : `y`
enregistre et continue, `n` abandonne, `c` (ou `F10`) annule.

Touches dans la session :

- La frappe insère au curseur ; `Insert` bascule le mode
  remplacement (`OVR` sur la ligne d'état).
- `Enter` scinde la ligne ; `Backspace` et `Delete` suppriment des
  caractères et joignent les lignes en fin de ligne.
- Les flèches, `Home`, `End`, `PageUp`, `PageDown` déplacent le
  curseur ; la vue défile, horizontalement aussi, pour le suivre.
- `Tab` insère des espaces jusqu'au prochain arrêt de huit colonnes.
- `F1` affiche le résumé des touches, `F2` enregistre, `F3` répète la
  dernière recherche, `F10` ouvre le menu.

`Find...` cherche vers l'avant depuis le curseur, littéralement et en
respectant la casse, en reprenant au début à la fin du tampon ; une
recherche sans résultat signale `Match not found` et laisse le
curseur en place.

L'éditeur n'édite que des fichiers texte, et annonce exactement ce
qu'il change :

- Le fichier doit être du texte UTF-8 d'au plus 16 Mio ; tout le
  reste (un fichier binaire, un retour chariot isolé, un fichier trop
  grand) est refusé avec la raison indiquée — jamais ouvert en
  charabia.
- Les tabulations sont converties en espaces sur des arrêts de huit
  colonnes au chargement, et les fins de ligne CRLF deviennent LF ;
  chaque conversion est annoncée sur la ligne d'état, jamais
  appliquée en silence.
- La présence ou l'absence du saut de ligne final du fichier est
  préservée.

Un chargement ou un enregistrement refusé pendant la session est
signalé sur la ligne d'état et le tampon est conservé ; la session ne
meurt jamais à cause d'un fichier refusé. Chaque chemin est résolu et
contrôlé par le noyau sous l'identité de l'appelant — l'éditeur ne
détient aucune autorité particulière.

## OPTIONS

- `-h, -?` — afficher la courte aide de cette commande et quitter.

## EXIT STATUS

- `0` — la session s'est terminée par `File > Exit`, ou la courte
  aide a été affichée.
- `1` — le fichier nommé n'a pas pu être chargé (pas du texte, trop
  grand, ou refusé), ou le terminal a échoué ; la raison est écrite
  sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour la courte aide (une étiquette
  BCP-47 telle que `fr-FR`).
- `TERM` — le terminal pour lequel la session dessine ; une valeur
  inconnue ou absente se replie sur une base sûre.

## SEE ALSO

- `cat`
- `man`
