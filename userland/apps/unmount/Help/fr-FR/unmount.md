## NAME

unmount — détacher un volume monté

## SYNOPSIS

`unmount [option...] name`

## DESCRIPTION

Met hors service le volume monté sous `name` : le système de fichiers
et le périphérique sont vidés, le montage sous `/Storage` est retiré
et la racine durable `id::` du volume est révoquée. `name` est le nom
de catalogue du volume (`usb1`) ou son point de montage
(`/Storage/usb1`), comparé à la liste des montages de l'API
d'informations système.

Un volume dont le périphérique a été retiré alors qu'il portait
encore des écritures non validées reste visible comme
`unavailable-dirty` (ou `unavailable-lost`), et un `unmount` simple
refuse : les données retenues sont conservées pour une réinsertion
vérifiée. `--force` est la sortie délibérée — les données retenues
sont abandonnées, le volume est retiré et la perte est consignée
dans le journal d'audit. Sur un volume sain, `--force` vide et
détache toujours proprement ; rien n'est abandonné quand une
validation propre est possible.

Le détachement exige l'autorité de montage (`CAP_FS_MOUNT`) ; le
noyau la vérifie et audite chaque décision. Les volumes de démarrage
permanents et les liaisons de vue du système ne se détachent pas.

## OPTIONS

- `-f, --force` — démontage forcé : retirer le volume même quand ses
  données ne peuvent pas être validées, en abandonnant les données
  retenues.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `unmount usb1` — détacher proprement le volume monté comme `usb1`.
- `unmount /Storage/usb1` — la même chose, nommé par son point de
  montage.
- `unmount --force usb1` — retirer un volume indisponible en
  abandonnant ses données retenues.

## EXIT STATUS

- `0` — le volume a été détaché (ou l'aide courte a été écrite).
- `1` — le volume est introuvable, non détachable, ou le noyau a
  refusé le détachement.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette
  BCP-47 telle que `fr-FR`).

## SEE ALSO

- `mount`
- `df`
- `man`
