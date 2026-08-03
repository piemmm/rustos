## NAME

sysinfo — 시스템 정보 질의하기

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

시스템 정보 API에 형식이 정해진 질의 하나를 보내고 응답을 그립니다.
TAIRiX에는 `/proc`도 `/sys`도 없습니다. 이 명령은 모든 프로그램이 쓰는,
버전이 매겨지고 능력 검사를 받는 같은 API의 터미널 얼굴이며, 능력 검사를
우회하는 경로는 없습니다.

질의:

- `processes`, `ps` — 프로세스를 나열합니다. 프로세스마다 한 줄.
- `memory`, `mem` — 커널 메모리 통계(`CAP_SYSINFO_KERNEL` 필요).
- `hardware`, `hw` — 감지된 하드웨어 트리(`CAP_SYSINFO_HW` 필요).
- `identity`, `id` — 기계 신원과 OS 버전.
- `uptime` — 부팅 이후 시간과 부팅 벽시계 시각.
- `limits`, `rlimits` — 유효한 자원 제한과 실시간 사용량.
- `seats` — 시트 목록: 각 디스플레이의 소유자와 전경 콘솔
  (`CAP_SYSINFO_HW` 필요).
- `pressure` — 실시간 메모리 압력 게이지: 밴드, 워터마크, 전환 카운터
  (`CAP_SYSINFO_KERNEL` 필요).
- `reclaim` — 회수 가능 캐시 장부, 클래스당 한 줄
  (`CAP_SYSINFO_KERNEL` 필요).
- `ramzip` — 압축 메모리 계층의 카운터 (`CAP_SYSINFO_KERNEL` 필요).
- `cpu` — CPU별 실행 큐 깊이, 컨텍스트 스위치, 선점
  (`CAP_SYSINFO_KERNEL` 필요).
- `irq`, `irqs` — 커널 IRQ 테이블: 바인딩된 인터럽트 라인마다 한 행 —
  라인 ID, 소유 드라이버 태스크, 부팅 이후 인터럽트 횟수, 그리고 라인의
  격리 여부 (`CAP_SYSINFO_HW` 필요).
- `cpuinfo` — CPU별 프로세서 보고서(`/proc/cpuinfo`의 상위 집합): 모델과
  제조사, 성능 등급, ISA 확장 플래그, 원시 식별 레지스터, 실측 코어 클록
  속도(MHz — 코어 클록 카운터가 없으면 정직하게 "unknown"), 그리고 고정
  기준·시간축 주파수. 공개된 하드웨어 사실이므로 케이퍼빌리티가 필요하지
  않습니다.
- `storage`, `io` — 볼륨별 저장소 I/O 건강 상태: 장애를 인식하는 블록 기반
  볼륨마다 한 행 — 영속 식별자의 앞부분, 이를 제공하는 블록 서비스 종단점,
  현재 가용성(available/degraded/recovering/lost), 그리고 고장 나거나
  불안정한 디스크가 드러나는 누적 결과 카운터(완료, 리셋, 타임아웃, 매체
  오류, 재발행) (`CAP_SYSINFO_KERNEL` 필요).
- `raid`, `arrays` — 구성된 RAID 배열과 배열 구성기가 보유한 장치: 배열마다
  한 행 — 식별자의 앞부분, 레벨, 건강
  상태(optimal/degraded/recovering/failed), 동기화된 멤버 수와 정의된 멤버
  수, 스트라이프 단위, 블록 수, 그리고 진행 중인 재구축이나 검사 — 이어서
  장치마다 한 행 — 하드웨어 트리 노드, 소속 배열(소속이 없는 후보는 대시),
  슬롯, 역할(candidate/held/in-sync/resyncing/faulted), 크기, 그리고 보유한
  메타데이터 세대 (`CAP_SYSINFO_HW` 필요).
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
