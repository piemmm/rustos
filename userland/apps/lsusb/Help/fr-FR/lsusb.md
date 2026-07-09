## NAME

lsusb — lister les périphériques USB découverts

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Affiche, une ligne par interface USB découverte, les numéros de bus et
de périphérique de l'interface, son identifiant `vendor:product` et les
noms de son fabricant et de son produit. L'inventaire est l'arbre
matériel — l'inventaire unique des périphériques du système — lu à
travers l'API d'informations système, qui exige la capacité
`CAP_SYSINFO_HW` ; un refus est signalé sur la sortie d'erreur standard
et rien n'est listé à sa place.

Les noms proviennent de l'instantané vérifié de la base publique
d'identifiants USB que cette commande embarque dans son propre paquet.
Une identité que la base ne nomme pas n'affiche que sa forme numérique
`ID vvvv:pppp`, jamais inventée, et le nombre de tels périphériques est
noté sur le flux d'information standard (fd 3). Si la table embarquée
est absente ou invalide, l'affichage se replie sur les identifiants
bruts avec la raison sur la sortie d'erreur standard — l'inventaire
lui-même reste listé.

RustOS n'a pas de registre Linux de numéros de bus/périphérique : le
numéro de bus d'un périphérique est l'identifiant de nœud stable de son
contrôleur dans l'arbre matériel et son numéro de périphérique est son
propre identifiant de nœud, et `-s` sélectionne ces identifiants (une
divergence délibérée et documentée par rapport au `lsusb` de Linux).
L'inventaire enregistre un nœud par *interface* : un périphérique à
plusieurs interfaces apparaît une fois par interface.

## OPTIONS

- `-v` — après chaque périphérique, afficher sa classe, sa sous-classe
  et son protocole d'interface (`bInterfaceClass`,
  `bInterfaceSubClass`, `bInterfaceProtocol`) avec les noms des tables
  de classes USB.
- `-t` — afficher les périphériques en arbre sous leurs contrôleurs et
  leurs bus.
- `-d [<vendor>]:[<product>]` — ne lister que les périphériques
  correspondant aux identifiants fabricant/produit donnés (hex) ; une
  moitié omise correspond à tout.
- `-s [[<bus>]:][<devnum>]` — ne lister que les périphériques
  correspondant aux identifiants de nœud du contrôleur (bus) et/ou du
  périphérique (décimal) ; une valeur sans deux-points est un numéro de
  périphérique seul.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `lsusb` — chaque périphérique USB découvert, avec ses noms.
- `lsusb -v` — la même chose, avec l'identité de classe de chaque
  interface.
- `lsusb -s 2:` — chaque périphérique sous le nœud contrôleur 2.
- `lsusb -d 046d:` — chaque périphérique du fabricant `046d`
  (Logitech).
- `lsusb -t` — les périphériques sous leur topologie de bus.

## EXIT STATUS

- `0` — la liste (ou l'aide courte) a été écrite.
- `1` — la requête de l'arbre matériel a été refusée ou a échoué, ou
  la sortie n'a pas pu être écrite.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (un identifiant
  BCP-47 tel que `fr-FR`).

## SEE ALSO

- `lspci`
- `sysinfo`
- `man`
