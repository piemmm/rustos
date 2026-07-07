## NAME

true — 아무것도 하지 않고 성공하기

## SYNOPSIS

`true [ignored arguments]`

## DESCRIPTION

모든 인수를 무시하고 상태 `0`으로 종료합니다. 항상 성공하는 명령이 필요한
곳 — 자리 표시 명령, 항상 참인 조건, 반복문의 본문 — 에서 스크립트가
사용합니다.

**첫 번째** 인수로 주어진 `-h`, `-?`, `--help`만 인정됩니다(GNU `true`가
`--help`를 인정하는 위치). 그 뒤의 어느 위치에서든 이 토큰들은 다른 모든
것과 마찬가지로 무시됩니다.

## OPTIONS

- `-h, -?` — (첫 번째 인수일 때만) 이 명령 자체의 짧은 도움말을
  표시합니다.

## EXAMPLES

- `true` — 성공합니다.
- `while true; do …; done` — 중단될 때까지 반복합니다.

## EXIT STATUS

- `0` — 항상(그것이 이 도구의 전부입니다).
- `1` — 요청된 짧은 도움말을 쓸 수 없었습니다.

## ENVIRONMENT

- `LANG` — 짧은 도움말의 선호 로캘(`ko-KR` 같은 BCP-47 태그).

## SEE ALSO

- `false`
- `man`
