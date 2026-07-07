## NAME

false — 아무것도 하지 않고 실패하기

## SYNOPSIS

`false [ignored arguments]`

## DESCRIPTION

모든 인수를 무시하고 상태 `1`로 종료합니다. 항상 실패하는 명령이 필요한
곳 — 항상 거짓인 조건, 의도된 실패 — 에서 스크립트가 사용합니다.

**첫 번째** 인수로 주어진 `-h`, `-?`, `--help`만 인정됩니다(GNU `false`가
`--help`를 인정하는 위치). 그 뒤의 어느 위치에서든 이 토큰들은 다른 모든
것과 마찬가지로 무시됩니다. 여전히 `1`로 종료하는 GNU `false --help`와
달리, 여기서는 짧은 도움말이 제공되면 `0`으로 종료합니다 — RustOS의 짧은
도움말 관례입니다.

## OPTIONS

- `-h, -?` — (첫 번째 인수일 때만) 이 명령 자체의 짧은 도움말을
  표시합니다.

## EXAMPLES

- `false` — 실패합니다.
- `until false; do …; done` — 본문을 한 번 실행합니다(조건이 항상
  거짓이므로).

## EXIT STATUS

- `1` — 항상(그것이 이 도구의 전부입니다).
- `0` — 요청된 짧은 도움말이 제공되었습니다.

## ENVIRONMENT

- `LANG` — 짧은 도움말의 선호 로캘(`ko-KR` 같은 BCP-47 태그).

## SEE ALSO

- `true`
- `man`
