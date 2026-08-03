## NAME

mdadm — inspecter et administrer les grappes RAID

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

Inspecte et administre les grappes RAID logicielles que le composeur de
grappes assemble à partir des périphériques membres. L'inventaire des
grappes et des périphériques est lu via l'API d'information système — la
même interface, au même niveau `CAP_SYSINFO_HW` que celui sous lequel
l'arbre matériel est lu. Les mutations de création, d'ajout, de retrait
et d'arrêt sont envoyées au point de contrôle du composeur, qui vérifie
que l'appelant détient `CAP_STORAGE_ADMIN` avant d'agir. Un refus est
signalé sur la sortie d'erreur avec un code de sortie non nul ; rien
n'est inventé et aucune autorité n'est présumée.

Exactement un mode est fourni par invocation.

TAIRiX n'a pas de `/dev`, donc les deux noms que Linux mdadm écrit sous
forme de fichiers de périphérique s'écrivent différemment ici — une
divergence délibérée et documentée :

- Un périphérique est nommé par l'identifiant de son nœud dans l'arbre
  matériel, écrit `node:<id>`, le même nom qu'affichent les rapports.
  Toute autre graphie est refusée plutôt que devinée.
- Une grappe est nommée par son identité de 128 bits en hexadécimal.
  L'identité complète de 32 chiffres est acceptée, tout comme tout
  préfixe désignant exactement une grappe ; un préfixe correspondant à
  plus d'une grappe est refusé plutôt que de deviner laquelle.

TAIRiX compose les niveaux RAID 0, 1, 5, 6, 10 et la triple parité. Il
n'a pas de RAID4, donc `--level=4` est refusé avec cette raison.

Un contexte consultatif concis — une grappe dégradée, ou des
périphériques vierges non affichés dans la vue des grappes — est écrit
sur le flux d'information standard (fd 3). Il est facultatif et ne
modifie jamais la sortie principale.

## OPTIONS

- `-C, --create` — créer une grappe sur les périphériques nommés et
  afficher l'identité que le composeur lui attribue.
- `-D, --detail` — indiquer l'identité, le niveau, la santé, le nombre
  de périphériques, la géométrie et toute position de reconstruction ou
  de vérification de chaque grappe. Sans opérande de grappe, indiquer
  toutes les grappes.
- `-E, --examine` — lister tous les périphériques que le composeur
  détient : les membres de grappes avec leur emplacement et leur état,
  et les périphériques vierges non affiliés sur lesquels une nouvelle
  grappe peut être créée.
- `-a, --add` — admettre un périphérique vierge dans un emplacement
  absent d'une grappe et le reconstruire.
- `-r, --remove` — retirer un périphérique membre d'une grappe.
- `-S, --stop` — arrêter une grappe active et libérer ses membres.
- `-l, --level=<level>` — le niveau à créer : `0`/`raid0`/`stripe`,
  `1`/`raid1`/`mirror`, `5`/`raid5`, `6`/`raid6`, `10`/`raid10`, ou
  `tp`/`raid-tp` pour la triple parité.
- `-n, --raid-devices=<count>` — le nombre d'emplacements de membres à
  créer ; il doit être égal au nombre d'opérandes de périphérique.
- `-c, --chunk=<blocks>` — l'unité de bande en blocs logiques ; valable
  uniquement pour un niveau à bandes.
- `-h, -?, --help` — afficher l'aide propre à cette commande.
- `-V, --version` — afficher la version et quitter.

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — créer une grappe RAID5 sur trois périphériques.
- `mdadm --detail` — indiquer toutes les grappes.
- `mdadm --examine` — lister tous les périphériques, membres et vierges.
- `mdadm --add 3f2a node:14` — ajouter un périphérique à la grappe dont l'identité commence par `3f2a`.
- `mdadm --stop 3f2a` — arrêter cette grappe.

## EXIT STATUS

- `0` — la requête a réussi (ou l'aide a été écrite).
- `1` — une capacité a été refusée, un nom n'a pas été résolu, le
  composeur a refusé la requête, ou la sortie n'a pas pu être écrite.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour cette aide (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
