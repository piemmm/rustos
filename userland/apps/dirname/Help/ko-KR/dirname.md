## NAME

dirname — 이름에서 마지막 구성 요소 떼어내기

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

각 경로 표기에서 마지막 구성 요소를 뗀 것을 인쇄합니다. 끝의 슬래시들을
제거한 뒤, 마지막 구성 요소와 그 앞의 슬래시들을 제거합니다. 이 수술은
순전히 어휘적입니다 — 어떤 경로도 해석되지 않고 디스크에 닿지도 않습니다.
남은 슬래시가 없는 표기의 부모는 `.`이고, 비어 버리는 부모는 루트입니다.

루트는 결코 벗겨지지 않습니다. `dirname /tools`는 `/`이고, TAIRiX 저장소
숲에서의 대응물로 `dirname Home:/tools`는 `Home:/`입니다. 별칭
루트(`Home:/`, `System:/`, …)는 POSIX 시스템에서 `/`가 하는 역할을 그대로
합니다.

## OPTIONS

- `-z, --zero` — 각 결과를 줄 바꿈 대신 NUL로 끝냅니다.
- `-h, -?` — 이 명령 자체의 짧은 도움말을 표시합니다.

## EXAMPLES

- `dirname /System/Commands/top.app` — `/System/Commands`를 인쇄합니다.
- `dirname src/lib.rs` — `src`를 인쇄합니다.
- `dirname file` — `.`을 인쇄합니다(디렉터리 부분이 없음).
- `dirname Home:/tools` — `Home:/`을 인쇄합니다(루트는 결코 벗겨지지
  않습니다).

## EXIT STATUS

- `0` — 결과(또는 짧은 도움말)를 썼습니다.
- `1` — 출력을 전달할 수 없었습니다.
- `2` — 명령줄을 이해하지 못했습니다.

## ENVIRONMENT

- `LANG` — 짧은 도움말의 선호 로캘(`ko-KR` 같은 BCP-47 태그).

## SEE ALSO

- `basename`
- `man`
