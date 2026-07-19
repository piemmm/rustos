## NAME

lspci — lister les périphériques PCI/PCIe découverts

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Affiche, une ligne par fonction PCI/PCIe découverte, un petit numéro
de liste, la classe de la fonction et les noms de son fabricant et de
son périphérique. L'inventaire est l'arbre matériel —
l'inventaire unique des périphériques du système — lu à travers l'API
d'informations système, qui exige la capacité `CAP_SYSINFO_HW` ; un
refus est signalé sur la sortie d'erreur standard et rien n'est listé
à sa place.

Les noms proviennent de l'instantané vérifié de la base publique
d'identifiants PCI que cette commande embarque dans son propre paquet.
Une identité que la base ne nomme pas est affichée sous sa forme
numérique (`Vendor 8086`, `Device 2922`, `Class 0106`), jamais
inventée, et le nombre de tels périphériques est noté sur le flux
d'information standard (fd 3). Si la table embarquée est absente ou
invalide, l'affichage se replie sur les identifiants numériques avec
la raison sur la sortie d'erreur standard — l'inventaire lui-même
reste listé.

TAIRiX n'enregistre pas d'adresse PCI `bus:device.function`. À la place,
chaque périphérique listé reçoit un petit numéro stable attribué dans
l'ordre du bus, affiché `#<n>`, et `-s` sélectionne ce numéro (une
divergence délibérée et documentée par rapport au `lspci` de Linux). Ce
numéro n'est *pas* l'identifiant de nœud interne de l'arbre matériel,
qui provient d'un espace réservé et peut être une grande valeur sans
signification. La vue `-k`
(pilote noyau) n'est pas encore proposée : le système ne publie pas de
registres de liaison de pilotes, et `lspci` ne rapporte que ce que le
système enregistre réellement.

## OPTIONS

- `-n` — identifiants numériques seuls : le code de classe et
  `vendor:device` en hexadécimal.
- `-nn` — les noms suivis des identifiants numériques entre crochets.
- `-v` — après chaque fonction, lister les ressources que son nœud
  déclare (fenêtres MMIO, lignes IRQ, ports d'E/S, contraintes DMA) —
  les demandes d'octroi de capacités enregistrées, pas l'état vivant.
- `-t` — afficher les fonctions en arbre sous leurs bus parents ;
  chaque ligne de bus intermédiaire nomme sa classe et son identité de
  clé de correspondance, et avec `-v` (`-tv`) montre aussi ses
  ressources déclarées.
- `-d [<vendor>]:[<device>]` — ne lister que les fonctions
  correspondant aux identifiants donnés (hexadécimal) ; une moitié
  omise correspond à tout.
- `-s <node>` — ne lister que la fonction portant le numéro de liste
  donné (le `#<n>` décimal affiché dans la liste).
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `lspci` — chaque fonction PCI découverte, avec ses noms.
- `lspci -nn` — la même chose, avec les identifiants numériques.
- `lspci -v -s 7` — la ligne du périphérique `#7` et ses ressources déclarées.
- `lspci -d 1af4:` — chaque fonction du fabricant `1af4` (virtio).
- `lspci -t` — les fonctions sous leur topologie de bus.

## EXIT STATUS

- `0` — la liste (ou l'aide courte) a été écrite.
- `1` — la requête de l'arbre matériel a été refusée ou a échoué, ou
  la sortie n'a pas pu être écrite.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
