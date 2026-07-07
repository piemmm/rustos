## NAME

sysinfo — 시스템 정보 질의하기

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

시스템 정보 API에 형식이 정해진 질의 하나를 보내고 응답을 그립니다.
RustOS에는 `/proc`도 `/sys`도 없습니다. 이 명령은 모든 프로그램이 쓰는,
버전이 매겨지고 능력 검사를 받는 같은 API의 터미널 얼굴이며, 능력 검사를
우회하는 경로는 없습니다.

질의:

- `processes`, `ps` — 프로세스를 나열합니다. 프로세스마다 한 줄.
- `memory`, `mem` — 커널 메모리 통계(`CAP_SYSINFO_KERNEL` 필요).
- `hardware`, `hw` — 감지된 하드웨어 트리(`CAP_SYSINFO_HW` 필요).
- `identity`, `id` — 기계 신원과 OS 버전.
- `uptime` — 부팅 이후 시간과 부팅 벽시계 시각.
- `limits`, `rlimits` — 유효한 자원 제한과 실시간 사용량.
- `help` — 이 명령 자체의 짧은 도움말.

질의가 없으면 짧은 도움말이 표시됩니다.

## OPTIONS

- `--all, -a` — `processes`와 함께: 자신의 것만이 아니라 시스템의 모든
  프로세스를 나열합니다. 서비스는 이 보기를 `CAP_SYSINFO_GLOBAL`을 지닌
  호출자에게만 허가합니다.
- `-h, -?` — 이 명령 자체의 짧은 도움말을 표시합니다.

## EXAMPLES

- `sysinfo identity` — 기계 신원과 OS 버전을 인쇄합니다.
- `sysinfo ps --all` — 시스템의 모든 프로세스를 나열합니다.

## EXIT STATUS

- `0` — 질의가 응답되고 그려졌습니다.
- `1` — 서비스가 거부했거나 실패했거나 결과를 전달할 수 없었습니다.
- `2` — 명령줄을 이해하지 못했습니다.

## ENVIRONMENT

- `LANG` — 짧은 도움말의 선호 로캘(`ko-KR` 같은 BCP-47 태그).

## SEE ALSO

- `man`
- `ps`
- `top`
