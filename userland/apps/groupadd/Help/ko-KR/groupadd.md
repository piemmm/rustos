## NAME

groupadd — 그룹 만들기

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

그룹 등록부에 그룹 하나를 추가합니다. 그룹 이름은 `[a-z_][a-z0-9_-]*`에
맞아야 하고 id는 십진 값입니다. 그룹 생성은 관리 작업입니다. 등록부는
사용자 관리 능력이 없는 호출자를 거부합니다.

`-g`를 생략하면 그룹 id는 기존 최고 id보다 하나 위로 자동
할당됩니다. 이미 쓰이고 있는 id 요청은 거부됩니다. 충돌의 권위자는
등록부입니다.

`--`는 옵션 해석을 끝내며, 이후의 모든 인수는 피연산자입니다.

## OPTIONS

- `-g, --gid GID` — 숫자 그룹 id. 생략하면 자동 할당(기존 최고 id보다
  하나 위).
- `-h, -?, --help` — 이 명령 자체의 짧은 도움말을 표시합니다.

## EXAMPLES

- `groupadd staff` — 자동 할당된 id로 `staff`를 만듭니다.
- `groupadd -g 100 staff` — id `100`으로 `staff`를 만듭니다.

## EXIT STATUS

- `0` — 그룹이 만들어졌습니다.
- `1` — 등록부가 생성을 거부했거나 실패했습니다(예: 능력 부재, 중복 id).
  이유가 표준 오류에 인쇄됩니다.
- `2` — 명령줄을 이해하지 못했습니다.

## ENVIRONMENT

- `LANG` — 짧은 도움말의 선호 로캘(`ko-KR` 같은 BCP-47 태그).

## SEE ALSO

- `useradd`
- `users`
