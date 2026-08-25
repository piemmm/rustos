## NAME

host — DNS로 이름 조회

## SYNOPSIS

`host [-t type] name|address`

## DESCRIPTION

시스템의 스텁 리졸버를 사용하여 도메인 이름을 해당 주소로 해석하고 각 응답을 한 줄
씩 출력합니다. `-t`가 없으면 `A`(IPv4)와 `AAAA`(IPv6) 레코드를 모두 조회하며,
`-t type`은 조회를 하나로 제한합니다.

조회할 재귀 DNS 서버는 시스템 정보 API를 통해 호스트 구성에서 읽어옵니다 —
`state:net/resolver/servers` 읽기가 보고하는 것과 동일한 활성 집합입니다 — 그리고
주소를 표시하기 전에 각 응답을 검증합니다. `/etc/resolv.conf`도 로컬 hosts 파일도
없습니다.

피연산자가 IPv4 또는 IPv6 주소 리터럴이면 **역방향** 조회입니다. 그 주소에 대응하는
`in-addr.arpa` / `ip6.arpa` 이름으로 다시 쓰이고, 기본 레코드 유형은 `PTR`이 되며,
찾은 레코드는 `<reverse-name> domain name pointer <name>.` 으로 출력됩니다.

`A`, `AAAA`, `PTR` 레코드만 지원합니다. 다른 유형(`MX`, `TXT` 등)은 조용히 `A`로
취급되지 않고 거부됩니다. 존재하지 않는 이름은 `Host <name> not found: 3(NXDOMAIN)`
을 출력하며, 어떤 서버에도 도달할 수 없으면 `host`는 표준 오류에 시간 초과를
보고합니다.

## OPTIONS

- `-t, --type` — 조회할 DNS 레코드 유형: `A`, `AAAA` 또는 `PTR`(대소문자 구분 안
  함). 지정하지 않으면 이름은 `A`와 `AAAA`를, 주소는 `PTR`을 조회합니다.
- `-?, --help` — 이 명령의 자체 간단 도움말을 표시합니다.

## EXAMPLES

- `host example.com` — 이름의 IPv4 및 IPv6 주소.
- `host -t AAAA example.com` — IPv6 주소만.
- `host 10.0.2.2` — 그 주소가 되돌아 가리키는 이름.

## EXIT STATUS

- `0` — 주소를 하나 이상 찾음(또는 간단 도움말을 출력함).
- `1` — 이름이 어떤 주소로도 해석되지 않음(부정 응답, 시간 초과 또는 리졸버 실패).
- `2` — 명령줄을 이해하지 못했거나 출력을 쓸 수 없음.

## ENVIRONMENT

- `LANG` — 간단 도움말에 선호되는 로케일(`fr-FR` 같은 BCP-47 태그).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
