## NAME

host — résoudre un nom via DNS

## SYNOPSIS

`host [-t type] name|address`

## DESCRIPTION

Résout un nom de domaine en ses adresses à l'aide du résolveur trivial du
système et affiche chaque réponse, une par ligne. Sans `-t`, les
enregistrements `A` (IPv4) et `AAAA` (IPv6) sont interrogés ; `-t type`
restreint la recherche à un seul.

Les serveurs DNS récursifs à interroger sont lus dans la configuration de
l'hôte via l'API d'information système — le même ensemble actif que le relevé
`state:net/resolver/servers` — et chaque réponse est validée avant qu'une
adresse ne soit affichée. Il n'y a pas de `/etc/resolv.conf` ni de fichier
d'hôtes local.

Un opérande qui est une adresse IPv4 ou IPv6 littérale est une recherche
**inverse** : il est réécrit en le nom `in-addr.arpa` / `ip6.arpa`
correspondant, le type par défaut devient `PTR`, et un enregistrement trouvé
s'affiche sous la forme `<reverse-name> domain name pointer <name>.`

Seuls les enregistrements `A`, `AAAA` et `PTR` sont pris en charge ; les
autres types (`MX`, `TXT`, etc.) sont refusés plutôt que traités
silencieusement comme `A`. Un nom qui n'existe pas affiche `Host <name> not
found: 3(NXDOMAIN)` ; lorsqu'aucun serveur n'est joignable, `host` signale un
dépassement de délai sur la sortie d'erreur.

## OPTIONS

- `-t, --type` — le type d'enregistrement DNS à interroger : `A`, `AAAA` ou
  `PTR` (insensible à la casse). Sans cette option, un nom interroge `A` et
  `AAAA`, et une adresse interroge `PTR`.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `host example.com` — les adresses IPv4 et IPv6 du nom.
- `host -t AAAA example.com` — seulement les adresses IPv6.
- `host 10.0.2.2` — le nom vers lequel cette adresse pointe.

## EXIT STATUS

- `0` — au moins une adresse a été trouvée (ou l'aide courte a été écrite).
- `1` — le nom n'a résolu aucune adresse (réponse négative, dépassement de
  délai ou échec du résolveur).
- `2` — la ligne de commande n'a pas été comprise, ou la sortie n'a pas pu
  être écrite.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle
  que `fr-FR`).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
