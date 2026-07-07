## NAME

rmdir — 빈 디렉터리 제거하기

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...`

## DESCRIPTION

각 디렉터리 피연산자를 순서대로 제거합니다. **빈 디렉터리**만
제거됩니다. 파일(또는 디렉터리가 아닌 어떤 것)과 내용이 있는 디렉터리는
파일 시스템 자체가 원자적으로 거부하므로, 그 자리에서 다른 무언가가
연결 해제되는 일은 결코 없습니다. 파일에는 `rm`을, 내용이 있는 트리에는
`rm -r`을 쓰십시오.

`-p`가 있으면 각 피연산자의 조상들도 가장 안쪽부터 제거됩니다.
`rmdir -p a/b/c`는 `a/b/c`, 그다음 `a/b`, 그다음 `a`를 제거합니다. 경로의
맨 루트(`/` 또는 `Home:/` 같은 별칭 루트)는 결코 제거 대상이 되지
않습니다.

`--ignore-fail-on-non-empty`가 있으면 「디렉터리가 비어 있지 않음」 거부는
오류가 아닙니다 — 그 피연산자(또는 `-p`의 상향 걸음)는 거기서 멈출
뿐입니다. 다른 어떤 거부도 용인되지 않습니다. 첫 진짜 실패가 이후의
피연산자로 가기 전에 실행을 멈춥니다. `--`는 옵션 해석을 끝내며, 이후의
모든 인수는 경로입니다.

## OPTIONS

- `-p, --parents` — 각 피연산자의 조상들도 가장 안쪽부터 제거합니다.
- `-v, --verbose` — 각 제거 시도를 `rmdir: removing directory, 'dir'`
  형식으로 보고합니다.
- `--ignore-fail-on-non-empty` — 비어 있지 않은 디렉터리는 오류가
  아닙니다. `-p`에서는 상향 걸음이 거기서 멈춥니다.
- `-h, -?` — 이 명령 자체의 짧은 도움말을 표시합니다(`--help`도 가능).

## EXAMPLES

- `rmdir Scratch` — 빈 디렉터리 하나를 제거합니다.
- `rmdir -p Projects/os/build` — 가장 안쪽부터 사슬을 제거합니다.
- `rmdir -p --ignore-fail-on-non-empty a/b` — `a/b`를 제거하고, 그로써
  `a`가 비면 `a`도 제거합니다.

## EXIT STATUS

- `0` — 모든 제거가 성공했습니다(`--ignore-fail-on-non-empty`가 용인한
  거부는 실패가 아닙니다).
- `1` — 파일 시스템 또는 출력 실패. 이유가 표준 오류에 인쇄됩니다.
- `2` — 명령줄을 이해하지 못했습니다.

## ENVIRONMENT

- `LANG` — 짧은 도움말의 선호 로캘(`ko-KR` 같은 BCP-47 태그).

## SEE ALSO

mkdir, rm, ls
