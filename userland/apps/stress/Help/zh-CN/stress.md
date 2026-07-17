## NAME

stress — 按需给机器的 CPU、内存、磁盘和缓存施加负载

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

按照既有工具 `stress`/`stress-ng` 的风格，启动刻意给机器施加负载的
工作进程：CPU 循环（`--cpu`）、内存分配并触碰的工作者（`--vm`）、
小缓冲区写入/同步（`--io`）、大块顺序磁盘写入（`--hdd`），以及搅动
缓存的重读者（`--cache`，TAIRiX 特有的补充）。每个工作者都是独立的
可换出进程；控制进程固定自己的内存（`mem_pin`，需要
`CAP_MEM_PIN`），以便在自己制造的压力下保持响应，并观察
`Ctrl-C`/`Terminate`，因此无论运行以完成、超时还是信号结束，都会
停止并回收工作者，删除全部临时文件。

内存和磁盘目标量按机器本身测算：除非 `--vm-bytes`/`--hdd-bytes`
给出明确数字，vm 工作者共享所发现内存的一半，hdd 工作者共享工作卷
可用空间的一半。`--overcommit P` 把这些发现的目标重设为资源的 `P`
个百分点；超过 100 时，工作者会推入压力区，由此产生的类型化拒绝
（卷已满、资源上限）被计数并作为预期结果报告 —— 绝不重试，绝不
崩溃。给机器施加负载不需要超出调用者自身资源限制的任何特权 ——
限制本身就是防线，`stress` 尊重它们。

接触磁盘的工作者只写在工作目录之下 —— 除非 `--temp-path` 指定其他
位置，否则是应用专属的用户缓存目录（`$HOME/Library/stress`）——
并且每个临时文件都会在收尾时删除，包括信号路径。

运行结束时会打印摘要（`--quiet` 抑制），并向咨询性的标准信息流
（fd 3）发出机器可读的 `summary` 记录。

## OPTIONS

- `--cpu N`、`--io N`、`--vm N`、`--hdd N` —— 启动 `N` 个指定种类的
  工作者，含义同 GNU `stress`。
- `--cache N` —— 启动 `N` 个缓存搅动工作者（TAIRiX 特有：反复的
  冷目录遍历与重读会推动内核的可回收缓存台账）。
- `--all N` —— 每种工作者各 `N` 个。
- `--vm-bytes B`、`--hdd-bytes B` —— 每个工作者的字节目标，支持
  GNU 后缀（`k`、`m`、`g`、`t`；如 `256M`）。默认值按发现的内存/
  可用空间测算。
- `--overcommit P` —— 把发现的 vm/hdd 目标设为资源的 `P` 个
  百分点；可以超过 100（此时的拒绝是预期结果）。
- `--timeout T` —— 在 `T` 之后停止（后缀 `s`/`m`/`h`；如 `5m`）。
  没有默认值：不指定时运行会持续到被信号结束。
- `--temp-path DIR` —— 接触磁盘的工作者的工作目录。
- `--monitor` —— 在运行期间于前台运行 `sysmon`；当监视器退出时
  报告本次运行。与 `--background` 相互矛盾。
- `-q, --quiet` —— 抑制 stdout 的摘要与进度行（错误仍会到达
  stderr）。
- `--background` —— 打印已分离控制进程的 PID 并交还提示符（隐含
  `--quiet`）。shell 的 `&` 作业形式同样可用；该标志面向脚本。
- `-h, -?, --help` —— 显示本命令自己的简短帮助并退出。
- `--version` —— 打印工具名称和版本并退出。

## EXIT STATUS

- `0` —— 运行完成（工作者的类型化拒绝是预期结果，不使其失败）。
- `1` —— 有工作者真正失败，或运行无法准备就绪。
- `2` —— 无法理解命令行。
- `130` / `143` —— `Ctrl-C` / `Terminate` 结束了运行，此前工作者已
  收尾、临时文件已删除。

## ENVIRONMENT

- `HOME` —— 决定默认工作目录（`$HOME/Library/stress`）。
- `LANG` —— 简短帮助的首选区域设置（如 `zh-CN` 的 BCP-47 标签）。

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
