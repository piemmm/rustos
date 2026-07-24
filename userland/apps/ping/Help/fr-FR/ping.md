## NAME

ping — envoyer des requêtes d'écho ICMP à un hôte réseau

## SYNOPSIS

`ping [option...] adresse`

## DESCRIPTION

Envoie des requêtes d'écho ICMP (IPv4) ou ICMPv6 (IPv6) à un hôte et
affiche chaque réponse avec son temps d'aller-retour, puis un bilan
final.

Les requêtes passent par une socket d'écho ICMP ouverte auprès de la
pile réseau en espace utilisateur, protégée par `CAP_NET` et `CAP_NET_RAW`
et journalisée. La pile détient l'identifiant d'écho, si bien qu'une
socket ne reçoit que les réponses à ses propres requêtes. Il n'y a pas de
résolution de noms dans cette version : la cible doit donc être une
adresse IPv4 ou IPv6 littérale ; un nom d'hôte est une erreur d'usage,
pas un échec silencieux.

Par défaut `ping` envoie une requête par seconde jusqu'à interruption ;
`-c` en borne le nombre. Chaque réponse indique la source, le numéro de
séquence et le temps ; une requête sans réponse dans le délai imparti
affiche une ligne d'expiration. Le bilan final indique les paquets émis
et reçus, le pourcentage de perte, et les temps d'aller-retour minimum,
moyen et maximum. `-q` n'affiche que l'en-tête et le bilan.

La durée de vie IP n'est pas exposée par l'interface de socket d'écho ;
contrairement à certaines implémentations de `ping`, une ligne de réponse
ne porte donc pas de champ `ttl=`.

## OPTIONS

- `-c, --count` — s'arrêter après ce nombre de requêtes.
- `-i, --interval` — secondes entre les requêtes (un décimal, p. ex. `0.5`).
- `-s, --size` — taille de la charge utile en octets.
- `-W, --timeout` — secondes d'attente de chaque réponse.
- `-w, --deadline` — délai global de l'exécution, en secondes.
- `-4, --ipv4` — exiger une cible IPv4.
- `-6, --ipv6` — exiger une cible IPv6.
- `-n, --numeric` — sortie numérique. Toujours active sur TAIRiX ;
  acceptée par familiarité.
- `-q, --quiet` — silencieux : seulement l'en-tête et le bilan final.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `ping 10.0.2.2` — envoyer des requêtes à un hôte IPv4 jusqu'à interruption.
- `ping -c 4 fe80::1` — envoyer quatre requêtes à un hôte IPv6.
- `ping -c 10 -i 0.2 10.0.0.1` — dix requêtes, une toutes les 200 ms.
- `ping -q -c 100 10.0.0.1` — exécution silencieuse, bilan seul.

## EXIT STATUS

- `0` — au moins une réponse reçue (ou l'aide courte affichée).
- `1` — aucune requête n'a reçu de réponse.
- `2` — ligne de commande incomprise, ou socket impossible à ouvrir.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `ss`
- `sysinfo`
- `man`
