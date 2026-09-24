## NAME

dns-sd — parcourir, résoudre et rechercher les services du lien local

## SYNOPSIS

`dns-sd [-t seconds] -B [type [domain]]`

`dns-sd [-t seconds] -L instance type [domain]`

`dns-sd [-t seconds] -G v4|v6|v4v6 host`

## DESCRIPTION

Interroge le réseau local — le lien — sur les services qu'il propose, par
l'intermédiaire du service de découverte du lien local du système. `dns-sd` ne
parle jamais lui-même le DNS multicast : le service de découverte interroge le
segment en son nom et admet chaque requête selon l'autorité propre du
programme.

Avec `-B`, il parcourt les instances d'un type de service, comme `_ipp._tcp`
pour les imprimantes, et affiche chaque instance à son ajout ou à son retrait.
Sans type, il liste tous les types de service que le lien propose. Avec `-L`,
il résout une instance en l'hôte et le port où elle se joint, et affiche ce
que l'instance dit d'elle-même (ses attributs `TXT`). Avec `-G`, il recherche
les adresses d'un hôte sous `local`.

Chaque réponse nomme l'interface sur laquelle elle a été apprise, et une ligne
`Flush` signifie que tout ce qui a été appris sur cette interface n'est plus
connu — son lien est tombé, ou le service a recommencé. Chaque nom affiché a
été choisi par une autre machine du lien ; il est donc affiché échappé, sous
la forme de présentation DNS : une espace en `\032`, un caractère de contrôle
par son code décimal.

Parcourir tous les types, ou un type que le compte courant n'a pas reçu,
exige `CAP_NET_DISCOVER_ALL`, que seul un compte administrateur porte. Le seul
domaine du lien est `local`.

## OPTIONS

- `-B` — parcourir les instances d'un type de service, ou tous les types si
  aucun n'est donné.
- `-L` — résoudre une instance d'un type de service.
- `-G` — rechercher les adresses IPv4 (`v4`), IPv6 (`v6`) ou les deux
  (`v4v6`) d'un hôte.
- `-t` — s'arrêter après ce nombre de secondes au lieu de tourner jusqu'à
  interruption.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `dns-sd -B _ipp._tcp` — les imprimantes du lien, à mesure qu'elles vont et
  viennent.
- `dns-sd -t 5 -B` — tous les types de service vus en cinq secondes.
- `dns-sd -L "Hall Printer" _ipp._tcp` — où se joint une imprimante.
- `dns-sd -G v4v6 printer.local` — les adresses d'un hôte.

## EXIT STATUS

- `0` — la commande est allée à son terme (ou l'aide courte a été écrite).
- `1` — le service de découverte a refusé la requête ou ne fonctionne pas.
- `2` — la ligne de commande n'a pas été comprise, ou la sortie n'a pas pu
  être écrite.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle
  que `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `man`
