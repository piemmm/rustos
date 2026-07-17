## NAME

basename — 이름에서 디렉터리와 접미사 떼어내기

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

각 경로 표기의 마지막 구성 요소를 인쇄합니다. 끝의 슬래시들을 제거한 뒤,
마지막으로 남은 슬래시까지(그 슬래시를 포함하여) 모두 제거합니다. 이
수술은 순전히 어휘적입니다 — 어떤 경로도 해석되지 않고 디스크에 닿지도
않습니다. `suffix`(두 번째 피연산자 또는 `-s`)가 있으면 끝의 `suffix`도
제거됩니다. 단, 그것이 남은 이름 전체인 경우는 예외입니다.

루트는 결코 벗겨지지 않습니다. `basename /`은 `/`이고, TAIRiX 저장소
숲에서의 대응물로 `basename Home:/`은 `Home:/`입니다. 별칭 루트(`Home:/`,
`System:/`, …)는 POSIX 시스템에서 `/`가 하는 역할을 그대로 합니다.

`-a`나 `-s`가 없으면 피연산자는 최대 두 개 — 이름과 선택적 접미사 — 만
받습니다. `-a`(또는 그것을 함의하는 `-s`)가 있으면 모든 피연산자가
이름입니다.

## OPTIONS

- `-a, --multiple` — 모든 피연산자를 이름으로 취급합니다.
- `-s, --suffix <suffix>` — 각 이름에서 끝의 `suffix`를 제거합니다.
  `-a`를 함의합니다. `--suffix=<suffix>`나 묶은 형태(`-s.rs`)로도 쓸 수
  있습니다.
- `-z, --zero` — 각 결과를 줄 바꿈 대신 NUL로 끝냅니다.
- `-h, -?` — 이 명령 자체의 짧은 도움말을 표시합니다.

## EXAMPLES

- `basename /System/Apps/top.app` — `top.app`을 인쇄합니다.
- `basename src/lib.rs .rs` — `lib`을 인쇄합니다.
- `basename -s .rs -a a.rs b.rs` — `a`와 `b`를 인쇄합니다.
- `basename Home:/` — `Home:/`을 인쇄합니다(루트는 결코 벗겨지지
  않습니다).

## EXIT STATUS

- `0` — 결과(또는 짧은 도움말)를 썼습니다.
- `1` — 출력을 전달할 수 없었습니다.
- `2` — 명령줄을 이해하지 못했습니다.

## ENVIRONMENT

- `LANG` — 짧은 도움말의 선호 로캘(`ko-KR` 같은 BCP-47 태그).

## SEE ALSO

- `dirname`
- `man`
