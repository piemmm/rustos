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
- `cpuinfo` — 每个 CPU 的处理器报告（`/proc/cpuinfo` 的超集）：型号与厂商、
  性能等级、ISA 扩展标志、原始标识寄存器、实测的核心时钟频率（以 MHz 表示
  ——没有核心时钟计数器时诚实地给出“unknown”），以及固定的参考／时基频率。
  这些是公开的硬件事实，无需任何能力。
- `storage`、`io` — 每个卷的存储 I/O 健康状况：每个具备故障感知的块支撑卷
  一行——其持久标识的前缀、为其服务的块服务端点、当前可用性
  （available/degraded/recovering/lost），以及使故障或抖动磁盘变得可见的
  累计结果计数器（完成、重置、超时、介质错误、重发）（需要
  `CAP_SYSINFO_KERNEL`）。
- `raid`、`arrays` — 已组建的 RAID 阵列以及阵列组建器所持有的设备：每个阵列
  一行——其标识前缀、级别、健康状况
  （optimal/degraded/recovering/failed）、已同步成员数与定义成员数、条带
  单元、块数，以及正在进行的重建或校验——随后每个设备一行——其硬件树节点、
  所属阵列（无归属的候选设备显示为短横线）、槽位、角色
  （candidate/held/in-sync/resyncing/faulted）、容量，以及它所携带的元数据
  世代（需要 `CAP_SYSINFO_HW`）。
- `show <resource-ref>` — 读取一个 `info:`/`state:`/`stats:`
  资源引用并打印其值。这些命名空间通过本 API 提供带类型的值，而不是字节流，因此
  `cat` 无法打开它们。被拒绝时会指出所需的能力。
- `describe <resource-ref>` — 打印响应信封而非值：生产者、提供时所依据的授权，
  以及载荷自身的元数据 —— 度量的种类、单位、复位行为与采样窗口；事实的类型与敏感度。
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
