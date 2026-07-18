## NAME

sysinfo — 查询系统信息

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

向系统信息 API 发出一个类型化查询并呈现回复。TAIRiX 没有 `/proc` 也没有
`/sys`：本命令是每个程序都使用的同一个带版本、经能力检查的 API 的终端面
孔，没有任何路径绕过能力检查。

查询：

- `processes`、`ps` — 列出进程，每个进程一行。
- `memory`、`mem` — 内核内存统计（需要 `CAP_SYSINFO_KERNEL`）。
- `hardware`、`hw` — 检测到的硬件树（需要 `CAP_SYSINFO_HW`）。
- `identity`、`id` — 机器身份和操作系统版本。
- `uptime` — 自启动以来的时间和启动的挂钟时间。
- `limits`、`rlimits` — 你的有效资源限制和实时使用量。
- `seats` — 席位清单：每个显示器的所有者及其前台控制台（需要
  `CAP_SYSINFO_HW`）。
- `pressure` — 实时内存压力仪表：档位、水位线和转换计数器（需要
  `CAP_SYSINFO_KERNEL`）。
- `reclaim` — 可回收缓存台账，每类一行（需要 `CAP_SYSINFO_KERNEL`）。
- `ramzip` — 压缩内存层的计数器（需要 `CAP_SYSINFO_KERNEL`）。
- `cpu` — 每个 CPU 的运行队列深度、上下文切换与抢占次数（需要
  `CAP_SYSINFO_KERNEL`）。
- `irq`、`irqs` — 内核 IRQ 表：每条已绑定的中断线一行——其编号、拥有它的
  驱动任务、自启动以来的中断次数，以及该线是否被隔离（需要
  `CAP_SYSINFO_HW`）。
- `help` — 本命令自身的简短帮助。

不带查询时显示简短帮助。

## OPTIONS

- `--all, -a` — 与 `processes` 一起：列出系统上的所有进程，而非只列你自
  己的；服务只把这一视图授予持有 `CAP_SYSINFO_GLOBAL` 的调用者。
- `-h, -?` — 显示本命令自身的简短帮助。

## EXAMPLES

- `sysinfo identity` — 打印机器身份和操作系统版本。
- `sysinfo ps --all` — 列出系统上的所有进程。

## EXIT STATUS

- `0` — 查询已回答并呈现。
- `1` — 服务拒绝或失败，或结果无法送达。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `man`
- `ps`
- `top`
