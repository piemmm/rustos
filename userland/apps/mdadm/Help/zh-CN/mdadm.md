## NAME

mdadm — 检查并管理 RAID 阵列

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

检查并管理阵列合成器由成员设备组装出的软件 RAID 阵列。阵列与设备清单
通过系统信息 API 读取——与读取硬件树相同的接口，相同的
`CAP_SYSINFO_HW` 门槛。创建、添加、移除与停止这些变更会发送到合成器的
控制端点，该端点在动作之前先检查调用者是否持有 `CAP_STORAGE_ADMIN`。
被拒绝时会在标准错误上报告并以非零状态退出；不会伪造任何内容，也不假
定任何权限。

每次调用只给出一种模式。

TAIRiX 没有 `/dev`，因此 Linux mdadm 以设备文件形式书写的那两个名称在
此处的写法不同——这是有意且已记录的差异：

- 设备以其硬件树节点 ID 命名，写作 `node:<id>`，与各报告打印的名称一
  致。任何其他写法都会被拒绝，而不是猜测。
- 阵列以其 128 位标识的十六进制形式命名。完整的 32 位标识可以接受，任
  何只指向一个阵列的前缀也可以；匹配多个阵列的前缀会被拒绝，而不是猜
  测指的是哪一个。

TAIRiX 可组合 RAID 级别 0、1、5、6、10 与三重校验。它没有 RAID4，因此
`--level=4` 会以该理由被拒绝。

简短的辅助上下文——降级的阵列，或阵列视图中未显示的空白设备——会写入标
准信息流（fd 3）。它是可选的，绝不改变主输出。

## OPTIONS

- `-C, --create` — 在指定的设备上创建阵列，并打印合成器为它铸造的标
  识。
- `-D, --detail` — 报告每个阵列的标识、级别、健康状况、设备计数、几何
  结构，以及任何正在进行的重建或校验位置。不给出阵列操作数时，报告每
  个阵列。
- `-E, --examine` — 列出合成器持有的每个设备：带有槽位与状态的阵列成
  员，以及可用于创建新阵列的未归属空白设备。
- `-a, --add` — 将一个空白设备纳入阵列缺失的槽位并重建它。
- `-r, --remove` — 将一个成员设备从阵列中退役。
- `-S, --stop` — 停止一个活动阵列并释放其成员。
- `-l, --level=<level>` — 要创建的级别：`0`/`raid0`/`stripe`、
  `1`/`raid1`/`mirror`、`5`/`raid5`、`6`/`raid6`、`10`/`raid10`，或表示
  三重校验的 `tp`/`raid-tp`。
- `-n, --raid-devices=<count>` — 要创建的成员槽位数量；它必须等于设备
  操作数的个数。
- `-c, --chunk=<blocks>` — 以逻辑块为单位的条带单元；仅对条带化级别有
  效。
- `-h, -?, --help` — 显示本命令自身的帮助。
- `-V, --version` — 打印版本并退出。

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — 在三个设备上创建一个 RAID5 阵列。
- `mdadm --detail` — 报告每个阵列。
- `mdadm --examine` — 列出每个设备，成员与空白设备皆列出。
- `mdadm --add 3f2a node:14` — 向标识以 `3f2a` 开头的阵列添加一个设备。
- `mdadm --stop 3f2a` — 停止该阵列。

## EXIT STATUS

- `0` — 请求成功（或已写出帮助）。
- `1` — 能力被拒绝、名称无法解析、合成器拒绝了请求，或输出无法写出。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 本帮助的首选区域设置（BCP-47 标签，如 `fr-FR`）。

## SEE ALSO

- `sysinfo`
- `man`
