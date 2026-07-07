## NAME

ps — 列出进程

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

通过系统信息 API 列出进程。默认只列出调用者自己的进程；服务用内核认证的调
用者身份检验每个查询范围，没有任何路径能绕过这一检查。

每个进程打印为列头之下的一行：进程 id（`PID`）、父进程 id（`PPID`）、属主
用户与组 id（`UID`、`GID`）、调度状态（`S`）、进程最后运行所在的 CPU
（`CPU`），以及命令名（`NAME`）。

`ps` 不接受操作数。

## OPTIONS

- `-e, -A, --all` — 列出系统上的所有进程，而非只列调用者自己的；服务只把
  这一视图授予持有 `CAP_SYSINFO_GLOBAL` 的调用者。
- `-h, -?` — 显示本命令自身的简短帮助。

## EXAMPLES

- `ps` — 列出你自己的进程。
- `ps -e` — 列出系统上的所有进程。

## EXIT STATUS

- `0` — 列表已写出。
- `1` — 服务拒绝或失败，或列表无法送达。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `man`
- `top`
- `sysinfo`
