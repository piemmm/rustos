## NAME

applib — administrer la bibliothèque de programmes du bureau

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

Administre la bibliothèque de programmes — le catalogue organisé en
dossiers d'applications lançables que le lanceur du bureau présente. La
bibliothèque est une donnée sur le volume, jamais une liste intégrée :
un magasin à l'échelle de la machine à
`/System/Settings/ProgramLibrary/library.conf` que chaque compte lit,
plus une superposition facultative par utilisateur au même chemin dans
le `Settings/` propre à l'utilisateur. Ce qu'un lanceur affiche est la
résolution des deux ensemble : les entrées et ajustements propres à
l'utilisateur l'emportent sur ceux à l'échelle de la machine.

Sans sous-commande (ou avec `list`), la bibliothèque résolue est
affichée dossier par dossier, une entrée par ligne : identifiant, nom
d'affichage et chemin du paquet — exactement ce que le lanceur affiche.
Les dossiers sont l'ensemble fermé `Accessories`, `Graphics`,
`Internet`, `Multimedia`, `Office`, `Programming`, `Games`,
`SystemTools`, `Utilities` et `Other` ; il n'y a pas de dossier de
forme libre.

`applib add` enregistre un paquet d'application. Son identité, son nom
d'affichage, son dossier et son icône sont tirés du manifeste
`AppInfo` signé du paquet ; `--category`, `--name` et `--icon`
remplacent le manifeste. Un paquet dont le manifeste ne déclare aucun
dossier de bibliothèque nécessite une `--category` explicite — l'outil
ne devine jamais. `applib remove` supprime un enregistrement, nommé par
son identifiant ou par le chemin du paquet avec lequel il a été
enregistré.

`applib hide` supprime une entrée de la bibliothèque résolue sans
supprimer son enregistrement — son identifiant reste revendiqué, de
sorte qu'un `rescan` ultérieur ne peut pas la ressusciter — et
`applib show` la réaffiche. Le masquage est une présentation, jamais
une autorité : le lancement d'un paquet est toujours régi par les
vérifications de signature et de capacité du chargeur, quel que soit le
catalogue.

`applib rescan` parcourt les magasins d'applications (`/System/Apps` et
`/Apps`, ou le propre `<home>/Apps` de l'appelant sous `--user`), lit le
manifeste de chaque paquet et enregistre chaque application qui demande
à être répertoriée et n'est pas encore cataloguée. Les enregistrements
existants — y compris les renommages et suppressions d'un conservateur —
ne sont jamais perturbés, et un paquet avec un manifeste illisible ou
malformé est sauté et compté, jamais une raison d'avorter. C'est ainsi
qu'une bibliothèque d'un système frais se peuple à partir des paquets
réellement installés, sans liste tenue à la main nulle part.

Par défaut, l'outil édite le magasin à l'échelle de la machine, que
seul un principal admis par la politique d'écriture de
`/System/Settings` peut modifier ; un compte ordinaire le lit mais le
personnalise via sa propre superposition avec `--user`. Une écriture
refusée indique sa raison et ne change rien.

En cas de succès, l'outil est silencieux sur la sortie standard ; le
résultat d'un changement est émis sous forme d'un enregistrement
consultatif structuré sur le flux d'informations standard (fd 3), que
les scripts peuvent capturer avec `3>records.jsonl` et que tout le
reste peut ignorer.

## OPTIONS

- `--category <folder>` — avec `list`, n'afficher que ce dossier ; avec
  `add`, classer l'entrée sous celui-ci (remplaçant la déclaration du
  manifeste).
- `--name <name>` — avec `add`, le nom d'affichage à afficher au lieu de
  celui du manifeste.
- `--icon <asset>` — avec `add`, l'icône (un nom de fichier à
  l'intérieur du `Resources/` du paquet) au lieu de celle du manifeste.
- `--user` — appliquer le changement à la superposition propre à
  l'appelant (ou, avec `rescan`, parcourir le propre `<home>/Apps` de
  l'appelant) au lieu du magasin à l'échelle de la machine.
- `-h, -?` — afficher la propre aide courte de cette commande.

## EXAMPLES

- `applib` — afficher la bibliothèque résolue, dossier par dossier.
- `applib list --category Games` — afficher un seul dossier.
- `applib add /Apps/chess.app` — enregistrer un paquet comme son
  manifeste le demande.
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  enregistrer un paquet qui ne déclare aucun référencement, sous un
  dossier explicite.
- `applib remove os.tairix.chess` — supprimer une entrée par
  identifiant.
- `applib hide os.tairix.chess --user` — la masquer de votre propre
  bibliothèque uniquement.
- `applib rescan` — enregistrer chaque paquet installé et répertorié pas
  encore dans le catalogue de la machine.

## EXIT STATUS

- `0` — le référencement, le changement, le rescan ou l'aide courte ont
  été terminés.
- `1` — un échec de magasin, de paquet ou de sortie (par exemple,
  l'appelant ne peut pas modifier le catalogue à l'échelle de la
  machine) ; la raison est indiquée sur le flux de diagnostic.
- `2` — la ligne de commande n'a pas été comprise, le dossier ou
  l'entrée est inconnu, ou le paquet ne peut pas être enregistré comme
  demandé.

## ENVIRONMENT

- `LANG` — la langue préférée pour l'aide courte (une balise BCP-47
  telle que `fr-FR`).
- `HOME` — le répertoire personnel de l'appelant : nomme la
  superposition par utilisateur et la racine de rescan `--user`
  `<home>/Apps`.

## SEE ALSO

- `man`
- `configure`
